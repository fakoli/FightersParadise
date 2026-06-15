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
//! cargo run -p fp-app -- validate <file.def>   # headless: load a character + print an
//!                                              #   actionable validation report (no window)
//! ```
//!
//! ## The `validate` subcommand (headless character linter)
//!
//! `validate <file.def>` loads a character through the same loader the live match
//! uses and prints an actionable report — missing referenced sprites, unresolved
//! `ChangeState`/`ChangeAnim` targets, expressions that failed to compile (silent
//! const-`0` fallbacks), and unsupported controllers — then exits 0 (the report
//! body carries the findings; a non-zero exit is reserved for a missing argument
//! or a character that cannot be loaded at all). It opens no window, GPU, or
//! audio device, so it runs anywhere. See [`validate`] for the analysis.
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

mod screens;
mod validate;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fp_audio::{AudioSystem, Sound};
use fp_character::{Character, LoadedCharacter, SoundRequest};
use fp_core::SpriteId;
use fp_engine::{EffectSide, Match, MatchInput, Player, RoundState, StageBounds, Winner};
use fp_formats::air::{AirFile, AnimAction};
use fp_formats::sff::SffFile;
use fp_render::{
    BlendMode, GlyphFont, PaletteTexture, Renderer, SpriteDrawParams, SpriteTexture, TextDrawParams,
};
use fp_stage::{BgLayer, Stage};
use fp_ui::{MatchHudState, ScreenpackHud, ScreenpackLayout, SelectDef, SystemDef};
use fp_input::{map_controller, Button as PadButton, ControllerInput, RawController, DEADZONE_DEFAULT};
use sdl2::controller::{Axis, Button as SdlPadButton, GameController};
use sdl2::event::Event;
use sdl2::keyboard::{Keycode, KeyboardState, Scancode};
use sdl2::GameControllerSubsystem;

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

/// Shipped original common-effects (`fightfx`) sprite/animation set (audit #17).
/// Loaded once per match to render common (`fightfx`) hit-sparks for KFM and
/// conventional characters. Resolved relative to the process working directory;
/// a missing/bad asset is a best-effort no-op (no spark, no panic).
const COMMON_FX_SFF: &str = "assets/data/fightfx.sff";
/// Shipped original common-effects animation file. See [`COMMON_FX_SFF`].
const COMMON_FX_AIR: &str = "assets/data/fightfx.air";

/// Shipped original HUD bitmap font (FL2b). A clean-room FNT v1 covering `0-9`,
/// `A-Z`, space, and colon, loaded once into a [`fp_render::GlyphFont`] to draw
/// the round/KO/winner announcer and the round timer as real text. Resolved
/// relative to the process working directory; a missing/bad font degrades the HUD
/// to its solid-color quad markers (no panic, no regression).
const HUD_FONT_FNT: &str = "assets/data/font.fnt";

/// The shipped default motif `system.def` (the title menu + select grid
/// geometry). Resolved relative to the process working directory. When absent or
/// unparseable the title menu degrades to a built-in fallback (VS / TRAINING /
/// EXIT) over the shipped trainingdummy roster — see [`screens::TitleMenu::fallback`].
const DEFAULT_SYSTEM_DEF: &str = "assets/data/system.def";

/// The shipped fallback `select.def` roster, used when the motif `system.def`
/// declares no usable `[Files] select` (or it cannot be read). The default motif
/// points its `select` at this same file.
const DEFAULT_SELECT_DEF: &str = "assets/data/select.def";

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

    // The `validate` subcommand is a headless CLI report — it must NOT open an
    // SDL2 window or a GPU device, so it is intercepted here, before `run()`.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).is_some_and(|a| a.eq_ignore_ascii_case("validate")) {
        std::process::exit(run_validate(&args));
    }

    if let Err(e) = run() {
        tracing::error!("Fatal error: {e}");
        std::process::exit(1);
    }
}

/// Runs the `validate <file.def>` subcommand: loads the character through the
/// real loader, prints an actionable report to stdout, and returns the process
/// exit code.
///
/// Per the task contract this **exits 0 with a summary** for any character that
/// loads (even one with authoring problems) — the report's body carries the
/// findings. A non-zero code is reserved for the two operational failures that
/// are not about the character's *content*: a missing `<file.def>` argument
/// (exit 2) and a character that cannot be loaded at all (exit 1, e.g. a missing
/// required SFF/AIR). The report itself is printed to stdout (its job is to be
/// the program's user-facing output); diagnostics go through `tracing`.
fn run_validate(args: &[String]) -> i32 {
    let Some(def_arg) = args.get(2) else {
        eprintln!("usage: fp-app validate <file.def>");
        return 2;
    };
    let def_path = Path::new(def_arg);
    match validate::validate(def_path) {
        Ok(report) => {
            // stdout: this IS the program's output (a user-facing report), so a
            // direct print is correct here — not logging.
            print!("{}", validate::render_report(&report));
            0
        }
        Err(e) => {
            tracing::error!("validate: cannot load {}: {e}", def_path.display());
            eprintln!("validate: failed to load {}: {e}", def_path.display());
            1
        }
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
    ///
    /// `override_rgba` is an optional `.act` palette override (the runtime
    /// costume-swap, FL2b): when `Some`, the sprite's GPU palette is built from it
    /// via [`PaletteTexture::from_override`] instead of the SFF-embedded palette;
    /// when `None` the embedded palette is used and the upload is byte-identical to
    /// before. The override is fixed per fighter (a CLI `--pN-pal` selection), so
    /// caching the resolved palette per sprite id is correct.
    fn get_or_create_sprite<'a>(
        &'a mut self,
        sff: &SffFile,
        sprite_id: SpriteId,
        renderer: &Renderer,
        override_rgba: Option<&[u8; 1024]>,
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
        // Build the GPU palette from the `.act` override when one is selected,
        // otherwise the SFF-embedded palette. With no override this is
        // byte-identical to `PaletteTexture::new(&palette_data)` (the default
        // render path is unchanged).
        let palette = PaletteTexture::from_override(
            renderer.device(),
            renderer.queue(),
            &palette_data,
            override_rgba,
        );

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
fn build_two_player_match(
    p1_def: &Path,
    p2_def: &Path,
    pal: PalSelection,
) -> fp_core::FpResult<Match> {
    let p1 = build_player(p1_def, P1_START_X, pal.p1)?;
    let p2 = build_player(p2_def, P2_START_X, pal.p2)?;
    // `Match::new` seeds facing toward each other from the start positions and
    // starts in the intro phase; the default 99-second round clock applies.
    Ok(Match::new(
        p1,
        p2,
        StageBounds::new(-STAGE_HALF_WIDTH, STAGE_HALF_WIDTH),
    ))
}

/// Loads one character `.def` into a [`Player`] positioned at `start_x`,
/// standing it in state 0 with control, with an optional `.act` palette override.
///
/// No app-side movement shim is applied: the loader supplies MUGEN's built-in
/// stand<->walk<->crouch<->jump locomotion for every character (task 7.3 part B),
/// and the [`Match`] runs each player's real [`fp_input::CommandMatcher`]
/// (task 7.3 part A) so `holdfwd`/`holdback`/… and walk velocity all fire from
/// the character's own data.
///
/// `pal_selection` (FL2b) is the `--pN-pal` choice: a 0-based index into the
/// character's loaded `.act` overrides, set as the entity's active palette so
/// [`draw_player`] renders that costume. `None` keeps the SFF-embedded palette
/// (unchanged). An out-of-range index is warn-logged and falls back to embedded
/// (the loader's [`LoadedCharacter::override_palette`] returns `None` for it).
fn build_player(
    def_path: &Path,
    start_x: f32,
    pal_selection: Option<usize>,
) -> fp_core::FpResult<Player> {
    tracing::info!("Loading match character: {}", def_path.display());
    let loaded = LoadedCharacter::load(def_path)?;

    let mut entity = Character::with_constants(loaded.constants);
    entity.pos = fp_core::Vec2::new(start_x, 0.0);
    entity.state_no = STATE_STAND;
    entity.ctrl = true;
    entity.anim = STATE_STAND; // action 0 == stand

    // Pick the costume palette. An explicit `--p1-pal`/`--p2-pal` flag wins;
    // otherwise fall back to the character's DEFAULT costume — MUGEN renders the
    // first `pal.defaults` / `pal1` `.act` palette by default, not the SFF-embedded
    // one. That default is what makes CvS-style characters (e.g. evilken, whose
    // embedded palette is a dark placeholder) render in real colors instead of
    // near-black. A character that ships no `.act` palettes (Kung Fu Man) yields
    // `None` here and keeps its SFF-embedded palette exactly as before.
    let pal_selection = pal_selection.or_else(|| loaded.default_palette_index());

    // Apply the chosen `.act` palette override (FL2b). A selection that the
    // character cannot honor (no such override slot) is warn-logged and left as
    // the embedded palette so the costume swap never crashes or shows blank.
    if let Some(sel) = pal_selection {
        if loaded.override_palette(Some(sel)).is_some() {
            entity.set_active_palette(Some(sel));
            tracing::info!("Applied .act palette override #{sel} to {:?}", loaded.name);
        } else {
            tracing::warn!(
                "requested palette #{sel} for {:?} is out of range ({} loaded); using embedded palette",
                loaded.name,
                loaded.palette_count()
            );
        }
    }

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

/// One field of [`MatchInput`] that a keyboard key drives.
///
/// Used by [`KEYBOARD_BINDINGS`] so the player-1 key map is a single explicit
/// data table (each row maps physical scancodes to the engine input bit they
/// assert) rather than scattered through the polling code, and so the table can
/// be asserted directly in unit tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputField {
    /// Absolute screen direction: up (jump).
    Up,
    /// Absolute screen direction: down (crouch).
    Down,
    /// Absolute screen direction: left.
    Left,
    /// Absolute screen direction: right.
    Right,
    /// Attack button `a` (light punch).
    A,
    /// Attack button `b` (medium punch).
    B,
    /// Attack button `c` (heavy punch).
    C,
    /// Attack button `x` (light kick).
    X,
    /// Attack button `y` (medium kick).
    Y,
    /// Attack button `z` (heavy kick).
    Z,
}

impl InputField {
    /// Asserts this field's bit in a [`MatchInput`].
    fn set(self, m: &mut MatchInput) {
        match self {
            InputField::Up => m.up = true,
            InputField::Down => m.down = true,
            InputField::Left => m.left = true,
            InputField::Right => m.right = true,
            InputField::A => m.a = true,
            InputField::B => m.b = true,
            InputField::C => m.c = true,
            InputField::X => m.x = true,
            InputField::Y => m.y = true,
            InputField::Z => m.z = true,
        }
    }
}

/// Player-1 keyboard key map: every physical [`Scancode`] that drives an engine
/// [`InputField`], as a single explicit table.
///
/// Movement uses **WASD or the arrow keys** (either source asserts the same
/// direction, so both bind to the same field); the six MUGEN attack buttons are
/// the **U/I/O** (punch row: `a` `b` `c`) and **J/K/L** (kick row: `x` `y` `z`)
/// home keys. The engine receives these as **absolute** screen directions and
/// resolves facing internally (inside the [`fp_input::CommandMatcher`]), so this
/// table stays a pure absolute-direction snapshot — do not pre-rotate here.
///
/// There is intentionally **no keyboard `start`/pause binding**: the engine's
/// `tick` takes no pause signal yet, matching the documented controller-Start
/// drop in [`controller_to_match_input`].
const KEYBOARD_BINDINGS: &[(Scancode, InputField)] = &[
    // Movement: WASD and the arrow keys both drive the same direction.
    (Scancode::W, InputField::Up),
    (Scancode::Up, InputField::Up),
    (Scancode::S, InputField::Down),
    (Scancode::Down, InputField::Down),
    (Scancode::A, InputField::Left),
    (Scancode::Left, InputField::Left),
    (Scancode::D, InputField::Right),
    (Scancode::Right, InputField::Right),
    // Punch row a/b/c.
    (Scancode::U, InputField::A),
    (Scancode::I, InputField::B),
    (Scancode::O, InputField::C),
    // Kick row x/y/z.
    (Scancode::J, InputField::X),
    (Scancode::K, InputField::Y),
    (Scancode::L, InputField::Z),
];

/// Builds a [`MatchInput`] from a held-key oracle, using [`KEYBOARD_BINDINGS`].
///
/// `is_held(scancode)` reports whether a physical key is currently held. Keeping
/// this pure (no SDL types) makes the player-1 key map unit-testable without a
/// live SDL context — the live path ([`match_input_from_keyboard`]) just supplies
/// the SDL keyboard state as the oracle. Multiple scancodes mapping to the same
/// field (WASD vs. arrows) are OR'd, so either source asserts the direction.
fn match_input_from_held(mut is_held: impl FnMut(Scancode) -> bool) -> MatchInput {
    let mut input = MatchInput::none();
    for &(scancode, field) in KEYBOARD_BINDINGS {
        if is_held(scancode) {
            field.set(&mut input);
        }
    }
    input
}

/// Builds a [`MatchInput`] (absolute screen directions + button presses) from the
/// current SDL2 keyboard state, using the player-1 [`KEYBOARD_BINDINGS`] key map
/// (WASD/arrows + U/I/O/J/K/L). The engine converts these to facing-relative
/// commands internally, so this stays a pure absolute-direction snapshot.
fn match_input_from_keyboard(keyboard: &KeyboardState<'_>) -> MatchInput {
    match_input_from_held(|scancode| keyboard.is_scancode_pressed(scancode))
}

/// Number of controller slots the app tracks. Slot 0 drives player 1 (alongside
/// the keyboard); slot 1, if filled, drives player 2 for a two-human match.
const CONTROLLER_SLOTS: usize = 2;

/// Live SDL game-controller state: the opened controllers plus the subsystem
/// needed to open more on hotplug.
///
/// The first [`CONTROLLER_SLOTS`] *attached* controllers occupy `slots`; a `None`
/// slot means "no device here" and yields a neutral input. Opening, polling, and
/// hotplug are all failure-tolerant — a missing or disconnected device is never a
/// panic, only a `tracing::warn!` and a fall-back to neutral.
struct Controllers {
    /// The SDL game-controller subsystem; kept alive so opened controllers stay
    /// valid and new ones can be opened when a device is plugged in.
    subsystem: GameControllerSubsystem,
    /// Up to [`CONTROLLER_SLOTS`] opened controllers, indexed by player slot.
    slots: [Option<GameController>; CONTROLLER_SLOTS],
}

impl Controllers {
    /// Wraps an opened game-controller subsystem and binds every already-connected
    /// controller-capable joystick that fits into a free player slot.
    ///
    /// The caller obtains the subsystem with `Sdl::game_controller()`; if that
    /// fails (no driver, headless), the caller simply runs keyboard-only and never
    /// constructs a `Controllers`. Construction here cannot fail — at worst all
    /// slots stay empty and every `input` call returns `None`.
    fn new(subsystem: GameControllerSubsystem) -> Self {
        let mut this = Self {
            subsystem,
            slots: [None, None],
        };
        // SDL must receive controller events through the shared event pump; this
        // is on by default but we assert it so polling state stays fresh.
        this.subsystem.set_event_state(true);
        this.open_all_connected();
        this
    }

    /// Scans every connected joystick and binds the controller-capable ones to
    /// free slots. Used at startup and is safe to re-run (it skips filled slots).
    fn open_all_connected(&mut self) {
        let count = match self.subsystem.num_joysticks() {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!("controller: could not enumerate joysticks: {e}");
                return;
            }
        };
        for index in 0..count {
            if self.subsystem.is_game_controller(index) {
                self.open_into_free_slot(index);
            }
        }
    }

    /// Opens the joystick at `joystick_index` and parks it in the first free
    /// slot. A failed open or a full slot table is logged and ignored.
    fn open_into_free_slot(&mut self, joystick_index: u32) {
        let Some(slot) = self.slots.iter().position(Option::is_none) else {
            tracing::info!(
                "controller: all {CONTROLLER_SLOTS} slots in use; ignoring joystick {joystick_index}"
            );
            return;
        };
        match self.subsystem.open(joystick_index) {
            Ok(ctrl) => {
                tracing::info!(
                    "controller: opened '{}' (instance {}) as player {}",
                    ctrl.name(),
                    ctrl.instance_id(),
                    slot + 1
                );
                self.slots[slot] = Some(ctrl);
            }
            Err(e) => {
                tracing::warn!("controller: failed to open joystick {joystick_index}: {e}");
            }
        }
    }

    /// Handles a `ControllerDeviceAdded` event (`which` is a *joystick index*).
    fn on_device_added(&mut self, joystick_index: u32) {
        if self.subsystem.is_game_controller(joystick_index) {
            self.open_into_free_slot(joystick_index);
        }
    }

    /// Handles a `ControllerDeviceRemoved` event (`which` is an *instance id*),
    /// freeing whichever slot held that controller so its slot can be reused.
    fn on_device_removed(&mut self, instance_id: u32) {
        for slot in &mut self.slots {
            let matches = slot
                .as_ref()
                .is_some_and(|c| c.instance_id() == instance_id);
            if matches {
                tracing::info!("controller: instance {instance_id} disconnected");
                *slot = None;
            }
        }
    }

    /// Returns the mapped [`ControllerInput`] for a player slot, or `None` when no
    /// controller is bound (or it has silently detached). Reading a detached
    /// controller never panics — it reports neutral, which we surface as `None`.
    fn input(&self, slot: usize) -> Option<ControllerInput> {
        let ctrl = self.slots.get(slot)?.as_ref()?;
        if !ctrl.attached() {
            return None;
        }
        Some(map_controller(&read_raw(ctrl), DEADZONE_DEFAULT))
    }
}

/// Reads one SDL [`GameController`]'s current physical state into the pure,
/// backend-agnostic [`RawController`] consumed by [`map_controller`]. This is the
/// only place SDL controller types touch the mapping; all the direction/button
/// logic lives in `fp-input` and is unit-tested without SDL.
fn read_raw(ctrl: &GameController) -> RawController {
    RawController {
        stick_x: ctrl.axis(Axis::LeftX),
        stick_y: ctrl.axis(Axis::LeftY),
        dpad_up: ctrl.button(SdlPadButton::DPadUp),
        dpad_down: ctrl.button(SdlPadButton::DPadDown),
        dpad_left: ctrl.button(SdlPadButton::DPadLeft),
        dpad_right: ctrl.button(SdlPadButton::DPadRight),
        face_south: ctrl.button(SdlPadButton::A),
        face_east: ctrl.button(SdlPadButton::B),
        face_west: ctrl.button(SdlPadButton::X),
        face_north: ctrl.button(SdlPadButton::Y),
        shoulder_left: ctrl.button(SdlPadButton::LeftShoulder),
        shoulder_right: ctrl.button(SdlPadButton::RightShoulder),
        start: ctrl.button(SdlPadButton::Start),
    }
}

/// Converts a mapped [`ControllerInput`] into a [`MatchInput`].
///
/// `MatchInput` has no `start` field (the engine's `tick` takes no pause signal),
/// so the controller's Start maps onto the input model's `start` slot only inside
/// `fp-input`; here it is intentionally dropped (documented no-op) until the
/// engine grows a pause path.
fn controller_to_match_input(c: &ControllerInput) -> MatchInput {
    MatchInput {
        up: c.direction.up,
        down: c.direction.down,
        left: c.direction.left,
        right: c.direction.right,
        a: c.button(PadButton::A),
        b: c.button(PadButton::B),
        c: c.button(PadButton::C),
        x: c.button(PadButton::X),
        y: c.button(PadButton::Y),
        z: c.button(PadButton::Z),
    }
}

/// Per-field OR of two [`MatchInput`]s, so a player can act from the keyboard
/// **or** a controller at the same time (either source asserting a bit wins).
fn merge_match_input(a: MatchInput, b: MatchInput) -> MatchInput {
    MatchInput {
        up: a.up || b.up,
        down: a.down || b.down,
        left: a.left || b.left,
        right: a.right || b.right,
        a: a.a || b.a,
        b: a.b || b.b,
        c: a.c || b.c,
        x: a.x || b.x,
        y: a.y || b.y,
        z: a.z || b.z,
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

/// Loads the shipped HUD bitmap font (FL2b) into a GPU [`GlyphFont`], or returns
/// `None` so the HUD falls back to its solid-color quad markers.
///
/// Resolves [`HUD_FONT_FNT`] relative to the process working directory and parses
/// it through the real [`fp_formats::fnt::FntFont`] loader. Best-effort: a missing
/// or unparseable font is logged and yields `None` — the HUD then draws the
/// round/KO/winner state as a colored quad exactly as before (no panic, no
/// regression). Split out (taking an explicit path via [`load_hud_font_from`]) so
/// the load path is unit-testable against the shipped asset by absolute path.
fn load_hud_font(renderer: &Renderer) -> Option<GlyphFont> {
    load_hud_font_from(renderer, Path::new(HUD_FONT_FNT))
}

/// Loads a HUD bitmap font from an explicit `.fnt` `path` into a [`GlyphFont`].
///
/// See [`load_hud_font`]. A missing file, a parse failure, or an unsupported FNT
/// version each returns `None` (logged), never a panic.
fn load_hud_font_from(renderer: &Renderer, path: &Path) -> Option<GlyphFont> {
    if !path.exists() {
        tracing::debug!(path = %path.display(), "HUD font not present; HUD uses quad markers");
        return None;
    }
    match fp_formats::fnt::FntFont::load(path) {
        Ok(font) => {
            tracing::info!(
                "HUD font loaded: {} glyphs, {}px tall from {}",
                font.glyph_count(),
                font.image_height,
                path.display()
            );
            Some(GlyphFont::new(renderer.device(), renderer.queue(), font))
        }
        Err(e) => {
            tracing::warn!(
                "HUD font {} failed to load: {e}; HUD uses quad markers",
                path.display()
            );
            None
        }
    }
}

/// A minimal HUD: per-fighter life bars, a smaller power (super-meter) bar under
/// each, the round announcer / KO / winner readout and the round timer as real
/// bitmap text, all drawn through the existing `RenderFrame` pipeline (life/power
/// bars as solid-color quads, text via `RenderFrame::draw_text`). Full lifebars
/// are a later phase.
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
    /// The shipped HUD bitmap font (FL2b), loaded once into a GPU [`GlyphFont`].
    /// `None` when `assets/data/font.fnt` is missing or fails to load — the HUD
    /// then falls back to the solid-color quad markers (no panic, no regression).
    font: Option<GlyphFont>,
}

impl Hud {
    /// Builds all HUD color quads on the GPU and loads the HUD font (best-effort).
    fn new(renderer: &Renderer) -> Self {
        Self {
            dark: HudColor::new(renderer, 40, 40, 48),
            green: HudColor::new(renderer, 60, 210, 90),
            red: HudColor::new(renderer, 220, 60, 60),
            yellow: HudColor::new(renderer, 240, 220, 60),
            white: HudColor::new(renderer, 240, 240, 240),
            blue: HudColor::new(renderer, 70, 150, 240),
            font: load_hud_font(renderer),
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

        // The round timer (remaining whole seconds), centered near the top, drawn
        // as bitmap text when the font is available. Always rendered during a live
        // fight; harmless during intro (counts the full clock).
        self.draw_announcer(frame, win_w, m);
    }

    /// Draws the round timer and the round/KO/winner announcer.
    ///
    /// When the HUD font loaded, both are drawn as real bitmap text via
    /// [`fp_render::RenderFrame::draw_text`]: the timer (remaining whole seconds)
    /// centered at the very top, and the announcer (`ROUND N` / `KO` / `P1 WINS` /
    /// `P2 WINS` / `DRAW`) centered just below it, tinted per state. With no font
    /// this falls back to the original solid-color quad marker (so a missing/bad
    /// font is no regression).
    fn draw_announcer(&self, frame: &mut fp_render::RenderFrame<'_>, win_w: f32, m: &Match) {
        const MARGIN: f32 = 12.0;
        /// Uniform text scale: the 7px-tall font drawn at 3x ≈ 21px glyphs.
        const TEXT_SCALE: f32 = 3.0;

        let announcer = round_label(m.round_state(), m.winner(), m.round_number());
        let timer = timer_text(m.timer());

        match self.font.as_ref() {
            Some(font) => {
                // Timer at the very top-center.
                draw_centered_text(frame, font, &timer, win_w, MARGIN, TEXT_SCALE, 1.0);
                // Announcer below the timer, only when there is something to say.
                if !announcer.is_empty() {
                    let line_h = font.line_height() as f32 * TEXT_SCALE;
                    let y = MARGIN + line_h + 6.0;
                    draw_centered_text(frame, font, &announcer, win_w, y, TEXT_SCALE, 1.0);
                }
            }
            // No font: fall back to the original centered colored quad marker for
            // the decided-round state (intro/fight show nothing — the bars carry
            // it), exactly as before this feature.
            None => {
                let marker = announcer_quad_color(self, m.round_state(), m.winner());
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

/// The round timer as HUD text: the remaining whole seconds (frames / 60),
/// clamped to non-negative, as a decimal string.
///
/// [`Match::timer`] is FRAMES remaining (60 Hz); MUGEN counts whole seconds, so
/// this integer-divides by 60 (flooring) — matching [`timer_frames_to_seconds`]
/// used by the screenpack HUD so both readouts agree. Pure and unit-tested.
fn timer_text(timer_frames: i32) -> String {
    timer_frames_to_seconds(timer_frames).to_string()
}

/// The on-screen pixel width of `text` in `font` at `scale`, summing each
/// glyph's destination advance from the pure layout (so centering matches what
/// `draw_text` actually draws).
///
/// Returns the rightmost glyph edge — the last placed glyph's `dst_x + width` —
/// scaled, or `0.0` for an empty/blank string. Pure given the font.
fn text_pixel_width(font: &GlyphFont, text: &str, scale: f32) -> f32 {
    font.layout(text)
        .iter()
        .map(|g| g.dst_x + g.width as f32)
        .fold(0.0f32, f32::max)
        * scale
}

/// Draws `text` horizontally centered in a `win_w`-wide window at top `y`, at the
/// given `scale` and `alpha`, using the normal blend mode. A no-op for empty
/// text. The centering uses [`text_pixel_width`] so it lines up with the glyphs
/// `draw_text` lays out.
fn draw_centered_text(
    frame: &mut fp_render::RenderFrame<'_>,
    font: &GlyphFont,
    text: &str,
    win_w: f32,
    y: f32,
    scale: f32,
    alpha: f32,
) {
    if text.is_empty() {
        return;
    }
    let w = text_pixel_width(font, text, scale);
    let x = ((win_w - w) / 2.0).max(0.0);
    frame.draw_text(
        font,
        text,
        &TextDrawParams {
            x,
            y,
            scale,
            alpha,
            blend: BlendMode::Normal,
        },
    );
}

/// The quad color for the decided-round announcer marker when no HUD font is
/// loaded (the pre-FL2b fallback): yellow on KO, green/red on a P1/P2 win, white
/// on a draw, and `None` during intro/fight (the bars carry that state). Kept as
/// a small helper so the fallback mapping is one place and matches the original.
fn announcer_quad_color(
    hud: &Hud,
    state: RoundState,
    winner: Option<Winner>,
) -> Option<&HudColor> {
    match (state, winner) {
        (RoundState::Ko, _) => Some(&hud.yellow),
        (RoundState::Win, Some(Winner::P1)) => Some(&hud.green),
        (RoundState::Win, Some(Winner::P2)) => Some(&hud.red),
        (RoundState::Win, _) => Some(&hud.white), // draw
        _ => None,
    }
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
        // Resolve the fighter's active `.act` palette override (FL2b): the runtime
        // selection (`Character::active_palette`) into the loaded override's RGBA,
        // or `None` to use the SFF-embedded palette (the default, unchanged).
        let override_rgba = player
            .loaded
            .override_palette(player.character.active_palette());
        cache.get_or_create_sprite(&player.loaded.sff, sprite_id, renderer, override_rgba);
    }
}

/// Decides the two-fighter draw order from their MUGEN sprite-draw priorities
/// (`sprpriority`, audit #16).
///
/// Returns `true` when P1 should be drawn **first** (i.e. *behind* P2), `false`
/// when P2 should be drawn first (P1 in front). The fighter with the **lower**
/// priority draws first (behind); the higher priority draws over it. Equal
/// priorities keep P1 behind P2 — a stable, deterministic default so the order
/// never flickers on a tie.
///
/// Pure decision split out from the draw loop so the ordering rule is unit-
/// testable without an SDL2 window or GPU.
#[must_use]
fn p1_draws_behind_p2(p1_sprpriority: i32, p2_sprpriority: i32) -> bool {
    p1_sprpriority <= p2_sprpriority
}

/// Draws one fighter from its current AIR frame at its world position and facing,
/// reading the already-populated per-character texture cache (see
/// [`cache_player_sprite`]). A missing frame or uncached sprite is skipped.
///
/// `camera_x` is the camera's world X; the fighter's world position is offset by
/// it before mapping to screen, so the playfield scrolls with the camera. With no
/// stage (`camera_x = 0`) this reduces to the original centered mapping, so the
/// flat-background path is unchanged.
fn draw_player(
    frame: &mut fp_render::RenderFrame<'_>,
    cache: &FighterRender,
    player: &Player,
    camera_x: f32,
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
    let screen_x = world_to_screen_x(player.pos().x - camera_x, win_w);

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

    // AfterImage trail (audit #33): draw a fading row of ghost frames BEHIND the
    // sprite first, so the live frame is drawn over them. We do not capture frame
    // history, so the trail re-uses the current frame, stepped back along the
    // character's facing with decaying alpha and the trail's color tint. Drawn
    // before the main sprite so it sits behind it.
    let afterimage = player.character.afterimage();
    if afterimage.is_active() {
        draw_afterimage_trail(
            frame, cached, &afterimage, draw_x, draw_y, flip_h, anim_frame.flip_v, facing_right,
        );
    }

    // PalFX color tint (audit #33): the character's active tint (identity when
    // none, so an untinted sprite is byte-identical to before this feature).
    let palfx = char_palfx_to_render(player.character.palfx());
    let params = SpriteDrawParams {
        x: draw_x,
        y: draw_y,
        flip_h,
        flip_v: anim_frame.flip_v,
        blend: render_blend,
        alpha,
        palfx,
        ..Default::default()
    };
    frame.draw_sprite(&cached.texture, &cached.palette, &params);
}

// ---------------------------------------------------------------------------
// Common-effects (fightfx) asset (audit #17)
// ---------------------------------------------------------------------------

/// The shipped common-effects (`fightfx`) sprite source and its GPU cache.
///
/// Pairs the loaded `fightfx.sff` with a [`FighterRender`] cache so common
/// ([`EffectSide::Common`]) hit-sparks can be decoded/drawn exactly like a
/// fighter's sprites. The matching `fightfx.air` is installed onto the
/// [`fp_engine::Match`] (via [`Match::set_common_fx`]) so the engine resolves the
/// spark frames; this struct only owns the sprite/texture side a renderer needs.
struct CommonFxRender {
    /// The decoded common-effects sprite file (`fightfx.sff`).
    sff: SffFile,
    /// GPU sprite cache for the common-effects sprites, decoded on first use.
    render: FighterRender,
}

/// Loads the shipped common-effects (`fightfx`) asset from the default paths,
/// returning the parsed AIR (to install on the [`Match`]) and the SFF render
/// bundle (for drawing).
///
/// Resolves the paths relative to the process working directory
/// ([`COMMON_FX_SFF`] / [`COMMON_FX_AIR`]) and delegates to
/// [`load_common_fx_from`]. Best-effort: a missing/unparseable asset yields
/// `None` (logged), so the match simply renders no common sparks — never a panic,
/// never a regression.
fn load_common_fx() -> Option<(AirFile, CommonFxRender)> {
    load_common_fx_from(Path::new(COMMON_FX_SFF), Path::new(COMMON_FX_AIR))
}

/// Loads a common-effects (`fightfx`) asset from explicit `sff_path`/`air_path`.
///
/// Both files are best-effort: a missing or unparseable `.sff`/`.air` returns
/// `None` (logged at debug/warn), so common sparks are simply disabled — never a
/// panic, never a regression. Split from [`load_common_fx`] so it is unit-testable
/// against the shipped asset by absolute path (test CWD is the crate dir).
fn load_common_fx_from(sff_path: &Path, air_path: &Path) -> Option<(AirFile, CommonFxRender)> {
    if !sff_path.exists() || !air_path.exists() {
        tracing::debug!(
            sff = %sff_path.display(),
            air = %air_path.display(),
            "common-fx asset not present; common hit-sparks disabled"
        );
        return None;
    }
    let sff = match SffFile::load(sff_path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("common-fx sff {} failed to load: {e}; common sparks disabled", sff_path.display());
            return None;
        }
    };
    let air = match AirFile::load(air_path) {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!("common-fx air {} failed to load: {e}; common sparks disabled", air_path.display());
            return None;
        }
    };
    tracing::info!(
        "common-fx loaded: {} sprites from {}",
        sff.sprites.len(),
        sff_path.display()
    );
    Some((
        air,
        CommonFxRender {
            sff,
            render: FighterRender::default(),
        },
    ))
}

// ---------------------------------------------------------------------------
// Hit-spark effect rendering (audit #17)
// ---------------------------------------------------------------------------

/// Ensures the GPU textures for every live hit-spark effect's current sprite are
/// cached (audit #17). Run once per frame **before** `begin_frame`, like
/// [`cache_player_sprite`], because decoding needs `&Renderer` while a live
/// [`fp_render::RenderFrame`] holds it borrowed. Each effect's sprite is decoded
/// from its owning side's SFF into that side's per-character cache. A
/// missing/undecodable sprite is a no-op (logged once by the cache).
fn cache_effect_sprites(run: &mut MatchRun, renderer: &Renderer) {
    // Collect (side, sprite) pairs first to avoid borrowing the match while the
    // per-side caches are mutated.
    let effects: Vec<(EffectSide, SpriteId)> = run
        .m
        .effects()
        .iter()
        .map(|fx| (fx.side, fx.sprite))
        .collect();
    for (side, sprite) in effects {
        // Hit-sparks always use their source SFF's embedded palette (no `.act`
        // costume swap on sparks); pass `None`. Spark sprite ids never collide
        // with a fighter's body sprites, so sharing the per-side cache is safe.
        match side {
            EffectSide::P1 => {
                run.p1_render
                    .get_or_create_sprite(&run.m.p1().loaded.sff, sprite, renderer, None);
            }
            EffectSide::P2 => {
                run.p2_render
                    .get_or_create_sprite(&run.m.p2().loaded.sff, sprite, renderer, None);
            }
            // A common spark draws from the shipped common-fx SFF into its own
            // cache. A no-op when no common-fx asset is loaded (no such effects
            // are ever spawned then).
            EffectSide::Common => {
                if let Some(fx) = run.common_fx.as_mut() {
                    fx.render.get_or_create_sprite(&fx.sff, sprite, renderer, None);
                }
            }
        }
    }
}

/// The sprite caches a hit-spark [`Effect`] may draw from, picked per effect by
/// its [`EffectSide`] (audit #17): the two fighters' caches and the optional
/// shared common-effects (`fightfx`) cache. Bundled so [`draw_effects`] stays a
/// small, readable call.
struct EffectRenders<'a> {
    /// Player 1's sprite cache (for [`EffectSide::P1`] sparks).
    p1_render: &'a FighterRender,
    /// Player 2's sprite cache (for [`EffectSide::P2`] sparks).
    p2_render: &'a FighterRender,
    /// The shared common-effects cache (for [`EffectSide::Common`] sparks), or
    /// `None` when no common-fx asset is loaded.
    common_render: Option<&'a FighterRender>,
}

/// Draws every live hit-spark effect at its world position, over the fighters
/// (audit #17).
///
/// Each effect's sprite was cached by [`cache_effect_sprites`] in the owning
/// source's cache (a fighter's, or the shared common-fx cache for common sparks).
/// This maps the effect's world position to screen (the same `world_to_screen_x`
/// plus ground-plane mapping the fighters use, offset by `camera_x`), anchors the
/// sprite by its axis, and draws it additively (a spark glows). A missing/uncached
/// sprite is skipped — never a panic. This draws only over the player/effect
/// region; it does not touch the HUD/screenpack.
///
/// `renders` bundles the three sprite caches a spark may source from (P1, P2, and
/// the optional shared common-fx cache) so the source is picked per effect.
fn draw_effects(
    frame: &mut fp_render::RenderFrame<'_>,
    renders: EffectRenders<'_>,
    m: &Match,
    camera_x: f32,
    win_w: f32,
    win_h: f32,
) {
    let EffectRenders {
        p1_render,
        p2_render,
        common_render,
    } = renders;
    let ground_y = win_h * 0.8;
    for fx in m.effects() {
        let cache = match fx.side {
            EffectSide::P1 => p1_render,
            EffectSide::P2 => p2_render,
            // A common spark draws from the shared common-fx cache; skip if the
            // common-fx asset is absent (no such effect would spawn then anyway).
            EffectSide::Common => match common_render {
                Some(c) => c,
                None => continue,
            },
        };
        let Some(cached) = cache.sprite_cache.get(&fx.sprite) else {
            continue;
        };
        // Map the spark's world anchor into screen space exactly like a fighter
        // (X centered + camera offset, Y on the ground plane), then anchor the
        // sprite by its axis and apply the AIR frame's authored offset.
        // TODO(audit #17): this is a minimal first cut — it draws at a fixed 1:1
        // scale with no facing mirror and ignores the AIR frame's other transforms
        // (per-frame scale/angle/flip and the spark's own blend value); a future
        // pass should honor those + the attacker's facing for the spark, as MUGEN
        // does. The fixed Additive blend below is likewise a deliberate placeholder.
        let screen_x = world_to_screen_x(fx.pos.x - camera_x, win_w);
        let draw_x = screen_x - cached.axis_x as f32 + fx.offset.x as f32;
        let draw_y = ground_y + fx.pos.y - cached.axis_y as f32 + fx.offset.y as f32;
        let params = SpriteDrawParams {
            x: draw_x,
            y: draw_y,
            blend: fp_render::BlendMode::Additive,
            ..Default::default()
        };
        frame.draw_sprite(&cached.texture, &cached.palette, &params);
    }
}

/// Converts the character-side [`fp_character::CurPalFx`] into the renderer's
/// [`fp_render::PalFx`] color tint (audit #33).
///
/// The `add`/`mul`/`color` fields pass straight through (both sides use the same
/// normalized float scale). No `is_active` gate is needed here: callers obtain the
/// effect from [`fp_character::Character::palfx`] /
/// [`fp_character::AfterImageState::palfx`], which already collapse an inactive
/// effect to [`fp_character::CurPalFx::IDENTITY`] — and that value's
/// `add`/`mul`/`color` are exactly [`fp_render::PalFx::IDENTITY`], so an inactive
/// effect still maps to the guaranteed no-op draw.
fn char_palfx_to_render(fx: fp_character::CurPalFx) -> fp_render::PalFx {
    fp_render::PalFx {
        add: fx.add,
        mul: fx.mul,
        color: fx.color,
    }
}

/// How far apart (in screen pixels) successive AfterImage ghost frames are
/// stepped behind the sprite. A small offset gives a readable motion smear
/// without scattering the trail across the screen.
const AFTERIMAGE_STEP_PX: f32 = 6.0;

/// The most ghost frames drawn for an AfterImage trail, regardless of the
/// controller's authored `length`. Caps per-frame draw cost.
const AFTERIMAGE_MAX_GHOSTS: i32 = 8;

/// Draws the AfterImage ghost trail behind a fighter's live sprite (audit #33).
///
/// We have no captured frame history, so the trail is faked from the *current*
/// frame: each ghost `i` (1-based, oldest last) is the same sprite stepped back
/// [`AFTERIMAGE_STEP_PX`] per ghost opposite the facing direction, drawn additive
/// with a linearly decaying alpha and the trail's [`palfx`](fp_character::AfterImageState::palfx)
/// color tint. Ghosts are drawn far-to-near so nearer (brighter) ghosts overlay
/// farther ones. The number of ghosts is the trail `length` clamped to
/// [`AFTERIMAGE_MAX_GHOSTS`]. Pure draw calls; never panics.
#[allow(clippy::too_many_arguments)]
fn draw_afterimage_trail(
    frame: &mut fp_render::RenderFrame<'_>,
    cached: &CachedSprite,
    afterimage: &fp_character::AfterImageState,
    base_x: f32,
    base_y: f32,
    flip_h: bool,
    flip_v: bool,
    facing_right: bool,
) {
    let ghosts = afterimage.length.clamp(1, AFTERIMAGE_MAX_GHOSTS);
    // Step trails opposite the facing direction (the sprite "leaves" them behind).
    let dir = if facing_right { -1.0 } else { 1.0 };
    let palfx = char_palfx_to_render(afterimage.palfx);
    // Draw the farthest (faintest) ghost first so nearer ghosts overlay it.
    for i in (1..=ghosts).rev() {
        // Decaying alpha: nearest ghost ~0.5, fading to ~0 at the tail.
        let t = i as f32 / (ghosts as f32 + 1.0);
        let alpha = 0.5 * (1.0 - t);
        let params = SpriteDrawParams {
            x: base_x + dir * AFTERIMAGE_STEP_PX * i as f32,
            y: base_y,
            flip_h,
            flip_v,
            blend: fp_render::BlendMode::Additive,
            alpha,
            palfx,
            ..Default::default()
        };
        frame.draw_sprite(&cached.texture, &cached.palette, &params);
    }
}

// ---------------------------------------------------------------------------
// Clsn hitbox/hurtbox debug overlay (audit #34)
// ---------------------------------------------------------------------------

/// The screen anchor a fighter's character-local geometry hangs off: the
/// screen-space pixel that its axis (`pos`) maps to. This is exactly the anchor
/// [`draw_player`] uses for the sprite (X from [`world_to_screen_x`], Y from the
/// ground plane plus the fighter's vertical offset), so boxes computed from it
/// line up with the drawn sprite. `ground_y` is the same `win_h * 0.8` fraction
/// used there. `camera_x` is the camera's world X, subtracted before mapping so
/// the boxes scroll with the sprite (matching [`draw_player`]).
fn player_screen_anchor(
    pos: fp_core::Vec2<f32>,
    camera_x: f32,
    win_w: f32,
    win_h: f32,
) -> (f32, f32) {
    let ground_y = win_h * 0.8;
    (world_to_screen_x(pos.x - camera_x, win_w), ground_y + pos.y)
}

/// Maps one character-local Clsn rect (Y-down, relative to the axis, as stored
/// in an AIR frame) into a screen-space [`fp_render::DebugBox`] of the given
/// color.
///
/// The transform mirrors `fp_physics::place_clsn`: the local X edges are
/// reflected about the axis when facing left (`anchor_x + sign * local_x`) and Y
/// is translated unchanged (`anchor_y + local_y`, Y down). `WORLD_TO_SCREEN`
/// being `1.0`, world and screen pixels share a scale, so no extra X scaling is
/// needed. The result is normalized to non-negative width/height.
fn clsn_to_screen_box(
    local: &fp_core::Rect,
    anchor_x: f32,
    anchor_y: f32,
    facing: fp_character::Facing,
    color: [f32; 4],
) -> fp_render::DebugBox {
    let sign = facing.sign() as f32;
    // Reflect both local X edges; left/right may swap when facing left.
    let lx0 = local.x;
    let lx1 = local.right();
    let sx0 = anchor_x + sign * lx0;
    let sx1 = anchor_x + sign * lx1;
    let (x, w) = if sx0 <= sx1 {
        (sx0, sx1 - sx0)
    } else {
        (sx1, sx0 - sx1)
    };
    // Y is never mirrored by facing.
    let y = anchor_y + local.y;
    let h = local.h.abs();
    fp_render::DebugBox { x, y, w, h, color }
}

/// Red (Clsn1 = attack box), MUGEN debug convention. RGBA in 0.0–1.0.
const CLSN1_COLOR: [f32; 4] = [1.0, 0.25, 0.25, 1.0];
/// Blue (Clsn2 = hurt/collision box), MUGEN debug convention. RGBA in 0.0–1.0.
const CLSN2_COLOR: [f32; 4] = [0.3, 0.55, 1.0, 1.0];

/// Draws one fighter's current-frame collision boxes when the debug overlay is
/// on: every Clsn2 (hurtbox, blue) first, then every Clsn1 (attack box, red) on
/// top so attack boxes read clearly where the two overlap. A missing frame draws
/// nothing.
fn draw_player_clsn(
    frame: &mut fp_render::RenderFrame<'_>,
    player: &Player,
    camera_x: f32,
    win_w: f32,
    win_h: f32,
) {
    let Some(anim_frame) = player_current_frame(player) else {
        return;
    };
    let (anchor_x, anchor_y) = player_screen_anchor(player.pos(), camera_x, win_w, win_h);
    let facing = player.facing();

    for hurt in &anim_frame.clsn2 {
        frame.draw_debug_box(&clsn_to_screen_box(
            hurt, anchor_x, anchor_y, facing, CLSN2_COLOR,
        ));
    }
    for attack in &anim_frame.clsn1 {
        frame.draw_debug_box(&clsn_to_screen_box(
            attack, anchor_x, anchor_y, facing, CLSN1_COLOR,
        ));
    }
}

// ---------------------------------------------------------------------------
// Stage background rendering (audit #29)
// ---------------------------------------------------------------------------

/// A loaded stage plus the GPU resources to draw its background layers: the
/// parsed [`fp_stage::Stage`] model, the stage's sprite (SFF) file, and a
/// lazily-populated per-sprite texture cache.
///
/// One [`StageRender`] is held per [`MatchRun`] when a stage is available; when
/// absent the app keeps its flat clear color (no regression). Drawing is split by
/// layer so the back layers render before the fighters and the front layers after
/// (see [`StageRender::draw_layer`]).
struct StageRender {
    /// The parsed stage definition (camera bounds, BG elements, …).
    stage: Stage,
    /// The stage's decoded sprite file (`[BGdef] spr`), used to draw BG elements.
    sff: SffFile,
    /// GPU sprite cache keyed by sprite id, decoded from `sff` on first use.
    sprite_cache: HashMap<SpriteId, CachedSprite>,
}

impl StageRender {
    /// Loads a stage from `path`: parses the `.def`, then loads its `[BGdef] spr`
    /// SFF. Returns `None` (with a log) when the stage cannot be parsed, declares
    /// no sprite file, or the SFF fails to load — the caller degrades to a flat
    /// clear color rather than failing the whole app. Never panics.
    fn load(path: &Path) -> Option<Self> {
        let stage = match Stage::load(path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("stage {} failed to parse: {e}; using flat background", path.display());
                return None;
            }
        };
        let Some(spr_path) = stage.bgdef.sprite_path.clone() else {
            tracing::warn!(
                "stage {} has no [BGdef] spr sprite file; using flat background",
                path.display()
            );
            return None;
        };
        let sff = match SffFile::load(&spr_path) {
            Ok(sff) => sff,
            Err(e) => {
                tracing::warn!(
                    "stage SFF {} failed to load: {e}; using flat background",
                    spr_path.display()
                );
                return None;
            }
        };
        tracing::info!(
            "stage loaded: {:?} ({} BG elements, {} stage sprites)",
            stage.info.name,
            stage.backgrounds.len(),
            sff.sprites.len(),
        );
        Some(Self {
            stage,
            sff,
            sprite_cache: HashMap::new(),
        })
    }

    /// Pre-decodes every BG element's sprite into the GPU cache. Run once per
    /// frame **before** `begin_frame` (decoding needs `&Renderer`, which a live
    /// [`fp_render::RenderFrame`] holds borrowed). Missing/oversized sprites are
    /// skipped (logged once via the cache), never fatal.
    fn cache_sprites(&mut self, renderer: &Renderer) {
        // Collect the sprite ids first to avoid borrowing `self.stage` while
        // `self.sprite_cache`/`self.sff` are mutated.
        let ids: Vec<SpriteId> = self
            .stage
            .backgrounds
            .iter()
            .filter_map(|bg| bg_sprite_id(bg.sprite.x, bg.sprite.y))
            .collect();
        for sprite_id in ids {
            cache_sff_sprite(&mut self.sprite_cache, &self.sff, sprite_id, renderer);
        }
    }

    /// Draws every BG element on `layer`, in file order, applying each element's
    /// parallax against the camera. A missing/uncached sprite is skipped.
    ///
    /// `camera_x` is the camera's world X (from [`Stage::camera_follow_x`]).
    /// Each element's world X after parallax is
    /// [`fp_stage::parallax_screen_x`]`(start.x, delta.x, camera_x)`, mapped into
    /// the window with the same [`world_to_screen_x`] the fighters use so the
    /// background and fighters share one coordinate frame. Vertical follow is not
    /// yet implemented, so `camera_y` is `0` and `start.y` is anchored to the same
    /// ground line the fighters stand on.
    fn draw_layer(
        &self,
        frame: &mut fp_render::RenderFrame<'_>,
        layer: BgLayer,
        camera_x: f32,
        win_w: f32,
        win_h: f32,
    ) {
        let ground_y = win_h * 0.8;
        for bg in self.stage.backgrounds.iter().filter(|b| b.layer == layer) {
            let Some(sprite_id) = bg_sprite_id(bg.sprite.x, bg.sprite.y) else {
                continue;
            };
            let Some(cached) = self.sprite_cache.get(&sprite_id) else {
                continue;
            };

            // World X after parallax, then into screen space (shared frame).
            let world_x = fp_stage::parallax_screen_x(bg.start.x, bg.delta.x, camera_x);
            let screen_x = world_to_screen_x(world_x, win_w);
            // Vertical follow is unimplemented (camera_y = 0); anchor start.y to
            // the ground line, Y down (matching the fighter draw convention).
            let screen_y = ground_y + fp_stage::parallax_screen_y(bg.start.y, bg.delta.y, 0.0);

            // Anchor by the sprite's axis like the fighters do.
            let draw_x = screen_x - cached.axis_x as f32;
            let draw_y = screen_y - cached.axis_y as f32;
            let params = SpriteDrawParams {
                x: draw_x,
                y: draw_y,
                ..Default::default()
            };
            frame.draw_sprite(&cached.texture, &cached.palette, &params);
        }
    }
}

/// Converts a BG element's `(group, image)` (stored as `i32` in the stage model)
/// into a [`SpriteId`], warning and returning `None` when either falls outside
/// the SFF `u16` range rather than wrapping to a wrong sprite.
fn bg_sprite_id(group: i32, image: i32) -> Option<SpriteId> {
    match (u16::try_from(group), u16::try_from(image)) {
        (Ok(g), Ok(i)) => Some(SpriteId::new(g, i)),
        _ => {
            tracing::warn!(
                "stage BG spriteno ({group}, {image}) is out of SFF range; skipping element"
            );
            None
        }
    }
}

/// Ensures the GPU textures for `sprite_id` are cached, decoding from `sff` on
/// first use. Shared by the stage background cache; mirrors
/// [`FighterRender::get_or_create_sprite`] but standalone so it can borrow a
/// cache map directly (the stage owns its own `HashMap`). Missing or undecodable
/// sprites are skipped with a warning, never a panic.
fn cache_sff_sprite(
    cache: &mut HashMap<SpriteId, CachedSprite>,
    sff: &SffFile,
    sprite_id: SpriteId,
    renderer: &Renderer,
) {
    if cache.contains_key(&sprite_id) {
        return;
    }
    let Some((index, sff_sprite)) = sff
        .sprites
        .iter()
        .enumerate()
        .find(|(_, s)| s.group == sprite_id.group() && s.image == sprite_id.image())
    else {
        tracing::warn!("stage sprite {sprite_id} not found in stage SFF; skipping");
        return;
    };
    let axis_x = sff_sprite.axis_x;
    let axis_y = sff_sprite.axis_y;
    let width = sff_sprite.width;
    let height = sff_sprite.height;
    let pal_idx = sff_sprite.palette_index as usize;
    if width == 0 || height == 0 {
        tracing::warn!("stage sprite {sprite_id} has zero dimensions; skipping");
        return;
    }
    let pixels = match sff.decode_sprite(index) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("failed to decode stage sprite {sprite_id}: {e}");
            return;
        }
    };
    let palette_data = match sff.palette(pal_idx) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("failed to load palette {pal_idx} for stage sprite {sprite_id}: {e}");
            return;
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
    cache.insert(
        sprite_id,
        CachedSprite {
            texture,
            palette,
            axis_x,
            axis_y,
        },
    );
}

// ---------------------------------------------------------------------------
// Intro / ending storyboard overlay (audit #32)
// ---------------------------------------------------------------------------
//
// A self-contained overlay path that plays a character's declared intro/ending
// storyboard over the match. It is *purely additive*: the normal intro timer,
// fighters, HUD, and screenpack are untouched, and when no storyboard (or its
// SFF) is available the overlay is simply absent — no regression to the normal
// intro. The driver lives in `fp-storyboard` (`StoryboardPlayer`); this layer
// only loads the SFF, caches GPU textures, and blits each `StoryboardDraw`.

/// The character-`.def` section MUGEN declares intro/ending storyboards under.
///
/// Both `intro.storyboard` and `ending.storyboard` live in the `[Arcade]` section
/// (the arcade-mode story bookends), not `[Files]`.
const STORYBOARD_SECTION: &str = "Arcade";

/// Which storyboard a character declares in its `.def` `[Arcade]` section.
///
/// MUGEN character `.def`s declare `intro.storyboard = <file>` and
/// `ending.storyboard = <file>`; these select which key to read.
#[derive(Debug, Clone, Copy)]
enum StoryboardKind {
    /// The `intro.storyboard` declaration — played during the pre-fight intro.
    Intro,
    /// The `ending.storyboard` declaration — played when the match is over.
    Ending,
}

impl StoryboardKind {
    /// The `[Arcade]` key this kind reads from the character `.def`.
    fn def_key(self) -> &'static str {
        match self {
            StoryboardKind::Intro => "intro.storyboard",
            StoryboardKind::Ending => "ending.storyboard",
        }
    }
}

/// Loads the declared intro/ending storyboard overlay for a character `.def`, or
/// `None` if the character declares none or it cannot be loaded.
///
/// `LoadedCharacter` does not retain the raw `.def` (and `fp-character` is not
/// ours to change for this task), so we re-read the small character `.def` here to
/// recover its `intro.storyboard`/`ending.storyboard` declaration, resolve it
/// relative to the `.def` directory, and hand it to [`StoryboardOverlay::load`].
/// Every failure path (unreadable `.def`, absent key, unloadable storyboard/SFF)
/// returns `None` so the match simply renders no extra overlay — never a panic,
/// never a regression to the normal intro.
fn load_character_storyboard(
    char_def: &Path,
    kind: StoryboardKind,
    renderer: &Renderer,
) -> Option<StoryboardOverlay> {
    let def = match fp_formats::def::DefFile::load(char_def) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(
                "could not re-read character def {} for storyboard: {e}",
                char_def.display()
            );
            return None;
        }
    };
    let rel = def
        .get(STORYBOARD_SECTION, kind.def_key())
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let sb_path = fp_formats::def::DefFile::resolve_path(char_def, rel);
    if !sb_path.exists() {
        tracing::warn!(
            "character {} declares {} = {rel} but {} is missing; skipping overlay",
            char_def.display(),
            kind.def_key(),
            sb_path.display()
        );
        return None;
    }
    StoryboardOverlay::load(&sb_path, renderer)
}

/// A playable storyboard overlay: a `fp-storyboard` [`StoryboardPlayer`] paired
/// with the storyboard's own sprite (SFF) file and a lazily-populated per-sprite
/// GPU texture cache.
///
/// Built best-effort with [`StoryboardOverlay::load`]; a missing/unparseable
/// storyboard `.def` or a missing/unloadable SFF yields `None`, and the caller
/// then renders nothing extra (the normal intro/match is unchanged). One overlay
/// is held per kind (intro, ending) on a [`MatchRun`].
struct StoryboardOverlay {
    /// The tick driver over the parsed storyboard scene model.
    player: fp_storyboard::StoryboardPlayer,
    /// The storyboard's own sprite container (`[SceneDef] spr`).
    sff: SffFile,
    /// GPU sprite cache keyed by sprite id, decoded from `sff` on first use.
    sprite_cache: HashMap<SpriteId, CachedSprite>,
    /// An opaque full-window clear quad behind the overlay, built once at load
    /// from the storyboard's start-scene `clearcolor` (or black when absent). The
    /// renderer exposes no per-draw quad recolor, so this is pre-built rather than
    /// tinted per frame; per-scene clearcolor changes are not tracked (a small,
    /// documented fidelity gap — the cutscene art itself is what reads).
    clear: HudColor,
}

impl StoryboardOverlay {
    /// Loads a storyboard overlay from a storyboard `.def` at `def_path`, resolving
    /// its `[SceneDef] spr` SFF relative to that `.def`'s directory.
    ///
    /// Returns `None` (with a log) when the storyboard has no scenes, declares no
    /// sprite file, or the SFF fails to load — the caller then draws nothing extra
    /// (no regression). `Storyboard::load` itself never panics; a parse problem
    /// degrades to an empty scene model which is rejected here. Never panics.
    ///
    /// `renderer` is used only to pre-build the opaque clear quad from the
    /// storyboard's start-scene `clearcolor`.
    fn load(def_path: &Path, renderer: &Renderer) -> Option<Self> {
        let storyboard = match fp_storyboard::Storyboard::load(def_path) {
            Ok(sb) => sb,
            Err(e) => {
                tracing::warn!(
                    "storyboard {} failed to load: {e}; skipping overlay",
                    def_path.display()
                );
                return None;
            }
        };
        if storyboard.scenes.is_empty() {
            tracing::warn!(
                "storyboard {} has no scenes; skipping overlay",
                def_path.display()
            );
            return None;
        }
        if storyboard.sprite_path.is_empty() {
            tracing::warn!(
                "storyboard {} has no [SceneDef] spr; skipping overlay",
                def_path.display()
            );
            return None;
        }
        // Resolve the SFF relative to the storyboard .def directory (same rule the
        // screenpack/stage loaders use).
        let sff_path = fp_formats::def::DefFile::resolve_path(def_path, &storyboard.sprite_path);
        let sff = match SffFile::load(&sff_path) {
            Ok(sff) => sff,
            Err(e) => {
                tracing::warn!(
                    "storyboard SFF {} failed to load: {e}; skipping overlay",
                    sff_path.display()
                );
                return None;
            }
        };
        tracing::info!(
            "storyboard overlay loaded: {} ({} scenes, {} sprites)",
            def_path.display(),
            storyboard.scenes.len(),
            sff.sprites.len(),
        );
        // Build the opaque clear backdrop from the start scene's clearcolor (the
        // first scene shown), defaulting to black when it declares none.
        let start = storyboard
            .start_scene
            .clamp(0, storyboard.scenes.len() as i32 - 1) as usize;
        let (cr, cg, cb) = storyboard
            .scenes
            .get(start)
            .and_then(|s| s.clearcolor)
            .unwrap_or((0, 0, 0));
        let clear = HudColor::new(renderer, cr, cg, cb);
        Some(Self {
            player: fp_storyboard::StoryboardPlayer::new(storyboard),
            sff,
            sprite_cache: HashMap::new(),
            clear,
        })
    }

    /// Whether the storyboard has finished playing (no more scenes).
    fn is_done(&self) -> bool {
        self.player.is_done()
    }

    /// Advances the storyboard one tick. A no-op once done.
    fn tick(&mut self) {
        self.player.tick();
    }

    /// Pre-decodes this tick's draw-list sprites into the GPU cache. Run once per
    /// frame **before** `begin_frame` (decoding needs `&Renderer`, which a live
    /// [`fp_render::RenderFrame`] holds borrowed). Missing/undecodable sprites are
    /// skipped (logged once via the cache), never fatal.
    fn cache_sprites(&mut self, renderer: &Renderer) {
        let ids: Vec<SpriteId> = self.player.draw_list().iter().map(|d| d.sprite).collect();
        for sprite_id in ids {
            cache_sff_sprite(&mut self.sprite_cache, &self.sff, sprite_id, renderer);
        }
    }

    /// Draws the storyboard's current frame: the pre-built opaque clear backdrop
    /// covering the whole window, then each [`StoryboardDraw`] mapped from
    /// storyboard-local coordinates into the window. A missing/uncached sprite is
    /// skipped.
    ///
    /// Storyboard-local coordinates use the `[Info] localcoord` frame (Y-down,
    /// origin top-left); this scales that frame to fill the window so the cutscene
    /// fills the screen regardless of the window size. Drawn entirely over the
    /// match (it is a full-screen cutscene), so the caller invokes it *after* the
    /// fighters/stage and instead of (not under) the normal scene.
    fn draw(&self, frame: &mut fp_render::RenderFrame<'_>, win_w: f32, win_h: f32) {
        // Cover the whole window with the clear color so the cutscene reads as a
        // full-screen overlay rather than compositing over the live fight.
        let cover = HudRect { x: 0.0, y: 0.0, w: win_w, h: win_h };
        let params = SpriteDrawParams {
            x: cover.x,
            y: cover.y,
            scale_x: cover.w.max(0.0),
            scale_y: cover.h.max(0.0),
            ..Default::default()
        };
        frame.draw_sprite(&self.clear.quad, &self.clear.palette, &params);

        // Map storyboard-local coords into the window. localcoord defaults to
        // (320, 240); guard against a zero/degenerate coordinate space.
        let (local_w, local_h) = self.player.storyboard().localcoord;
        let sx = if local_w > 0 { win_w / local_w as f32 } else { 1.0 };
        let sy = if local_h > 0 { win_h / local_h as f32 } else { 1.0 };

        for draw in self.player.draw_list() {
            let Some(cached) = self.sprite_cache.get(&draw.sprite) else {
                continue;
            };
            // Anchor by the sprite's axis like the fighters/stage do, then scale
            // the local-space position into the window.
            let screen_x = draw.pos.0 * sx - cached.axis_x as f32;
            let screen_y = draw.pos.1 * sy - cached.axis_y as f32;
            let (render_blend, alpha) = map_blend_mode(&draw.blend);
            let params = SpriteDrawParams {
                x: screen_x,
                y: screen_y,
                flip_h: draw.flip_h,
                flip_v: draw.flip_v,
                blend: render_blend,
                alpha,
                ..Default::default()
            };
            frame.draw_sprite(&cached.texture, &cached.palette, &params);
        }
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

/// The top-level launch route chosen from the (palette-flag-stripped) positional
/// CLI args. See [`cli_route`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CliRoute {
    /// Launch the in-app Title menu (no file args, or an explicit `menu`).
    Menu,
    /// A direct content path (`p1.def [p2.def]`, `file.sff [file.air]`, ...)
    /// handled by [`select_mode`] exactly as before — the menu is skipped.
    Direct,
}

/// Decides whether the (palette-flag-stripped) positional `args` launch the
/// in-app Title menu or a direct content view.
///
/// `args[0]` is the program name. The Title menu launches when there is **no**
/// file argument (a fresh clean-room run) or the first argument is an explicit
/// `menu` token; any other first argument (a `.def`/`.sff`/...) routes to the
/// legacy direct path so the existing CLI is preserved exactly. Pure and
/// unit-tested.
fn cli_route(args: &[String]) -> CliRoute {
    match args.get(1) {
        None => CliRoute::Menu,
        Some(a) if a.eq_ignore_ascii_case("menu") => CliRoute::Menu,
        Some(_) => CliRoute::Direct,
    }
}

/// Per-player `.act` palette selection from the `--p1-pal N` / `--p2-pal N` CLI
/// flags (FL2b). Each is a 0-based index into the character's loaded `.act`
/// overrides; `None` (the default) renders the SFF-embedded palette, so omitting
/// the flags is byte-identical to before this feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct PalSelection {
    /// Player 1's selected override index, or `None` for the embedded palette.
    p1: Option<usize>,
    /// Player 2's selected override index, or `None` for the embedded palette.
    p2: Option<usize>,
}

/// Parses (and strips) the `--p1-pal N` / `--p2-pal N` palette flags from `args`,
/// returning the selections plus the remaining positional args (program name +
/// file paths) that the rest of CLI routing consumes.
///
/// Each flag takes the next token as a non-negative decimal index; a missing or
/// non-numeric value drops that flag with a warning (the selection stays `None`).
/// Unknown `--…` tokens are passed through untouched. Pure — unit-tested without a
/// window or GPU.
fn parse_pal_flags(args: &[String]) -> (PalSelection, Vec<String>) {
    let mut sel = PalSelection::default();
    let mut rest: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        let which = if arg.eq_ignore_ascii_case("--p1-pal") {
            Some(true)
        } else if arg.eq_ignore_ascii_case("--p2-pal") {
            Some(false)
        } else {
            None
        };
        match which {
            Some(is_p1) => {
                // Consume the value token, if present and numeric.
                match args.get(i + 1).and_then(|v| v.parse::<usize>().ok()) {
                    Some(n) => {
                        if is_p1 {
                            sel.p1 = Some(n);
                        } else {
                            sel.p2 = Some(n);
                        }
                        i += 2;
                    }
                    None => {
                        tracing::warn!(
                            "{arg} expects a non-negative integer palette index; ignoring"
                        );
                        i += 1;
                    }
                }
            }
            None => {
                rest.push(arg.clone());
                i += 1;
            }
        }
    }
    (sel, rest)
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
    /// The loaded stage, when one is available. `None` keeps the flat clear-color
    /// background (no regression); `Some` renders parallax background layers
    /// behind/in front of the fighters with a camera following their midpoint.
    stage: Option<StageRender>,
    /// The loaded `fight.def` screenpack HUD, when one is available. `None` falls
    /// back to the hand-rolled quad [`Hud`] (no regression); `Some` draws the real
    /// lifebars/power/text via [`fp_ui::ScreenpackHud`] instead of the quads.
    screenpack: Option<ScreenpackHud>,
    /// P1's declared intro storyboard, played as a full-screen overlay during the
    /// first round's [`RoundState::Intro`] (audit #32). `None` when P1 declares no
    /// `intro.storyboard` or it/its SFF cannot load — then the normal intro plays
    /// with no overlay (no regression).
    intro_storyboard: Option<StoryboardOverlay>,
    /// P1's declared ending storyboard, played as a full-screen overlay once the
    /// match is decided (audit #32). `None` when P1 declares no `ending.storyboard`
    /// or it/its SFF cannot load.
    ending_storyboard: Option<StoryboardOverlay>,
    /// Whether the intro overlay has been played to completion this match, so it
    /// is shown once (during round 1's intro) and not re-triggered on later rounds.
    intro_storyboard_done: bool,
    /// The shipped common-effects (`fightfx`) sprite source + GPU cache, when
    /// loaded (audit #17). `None` when the asset is absent/bad — then common
    /// (`fightfx`) hit-sparks simply don't render (no panic, no regression). The
    /// matching AIR is installed on [`MatchRun::m`] via [`Match::set_common_fx`].
    common_fx: Option<CommonFxRender>,
    /// A full-color background image drawn behind the fighters, used when no MUGEN
    /// `[BGdef]`-style [`stage`](Self::stage) is loaded. Defaults to the shipped
    /// clean-room dojo backdrop so the match no longer plays over a flat grey
    /// clear color; `None` only if even that asset is absent (then the flat
    /// clear-color path is unchanged). A real loaded stage takes precedence and
    /// this stays `None`.
    background: Option<fp_render::ImageTexture>,
}

/// Which storyboard overlay (if any) is *active* for the current frame, given the
/// live round state (audit #32).
///
/// Pure decision split out from [`MatchRun`] so the gating rule is unit-testable
/// without an SDL2 window, a GPU, or a real `Match`:
/// - the **intro** plays during the first round's [`RoundState::Intro`], until it
///   has run to completion (`intro_done`), and only when one is loaded;
/// - the **ending** plays once the whole match is decided (`match_over`), and only
///   when one is loaded;
/// - otherwise no overlay is active and the normal match renders unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveStoryboard {
    /// No overlay this frame — render the normal match (no regression).
    None,
    /// Play the intro overlay this frame.
    Intro,
    /// Play the ending overlay this frame.
    Ending,
}

/// Decides which storyboard overlay is active for a frame from the round state and
/// per-match flags. See [`ActiveStoryboard`]. Pure (no `self`) so it is testable.
fn active_storyboard(
    round_state: RoundState,
    round_number: i32,
    match_over: bool,
    intro_done: bool,
    has_intro: bool,
    has_ending: bool,
) -> ActiveStoryboard {
    // The ending takes precedence once the match is decided.
    if match_over && has_ending {
        return ActiveStoryboard::Ending;
    }
    // The intro plays only during round 1's intro phase, before it has finished.
    if has_intro
        && !intro_done
        && round_number == 1
        && round_state == RoundState::Intro
    {
        return ActiveStoryboard::Intro;
    }
    ActiveStoryboard::None
}

impl MatchRun {
    /// Which overlay is active this frame (the gating decision), reading the live
    /// [`Match`] state and this run's loaded-overlay / done flags.
    fn active_storyboard(&self) -> ActiveStoryboard {
        active_storyboard(
            self.m.round_state(),
            self.m.round_number(),
            self.m.match_winner().is_some(),
            self.intro_storyboard_done,
            self.intro_storyboard.is_some(),
            self.ending_storyboard.is_some(),
        )
    }

    /// Advances the active overlay one tick and decodes its sprites for this frame
    /// (both gated behind [`MatchRun::active_storyboard`]). Run once per frame
    /// **before** `begin_frame`, because decoding needs `&Renderer`. When the intro
    /// overlay finishes it latches `intro_storyboard_done` so it is shown once.
    /// A no-op when no overlay is active — the normal match is untouched.
    fn tick_storyboard(&mut self, renderer: &Renderer) {
        match self.active_storyboard() {
            ActiveStoryboard::Intro => {
                if let Some(intro) = self.intro_storyboard.as_mut() {
                    intro.cache_sprites(renderer);
                    intro.tick();
                    if intro.is_done() {
                        self.intro_storyboard_done = true;
                    }
                }
            }
            ActiveStoryboard::Ending => {
                if let Some(ending) = self.ending_storyboard.as_mut() {
                    ending.cache_sprites(renderer);
                    ending.tick();
                }
            }
            ActiveStoryboard::None => {}
        }
    }

    /// The overlay to draw this frame, if one is active and not finished. Returns
    /// `None` when the normal match should render (no overlay), so the caller draws
    /// the storyboard *instead of* nothing extra and the rest of the scene is
    /// unchanged. A finished overlay returns `None` so play falls back to the
    /// normal view on the frame after it ends.
    fn storyboard_to_draw(&self) -> Option<&StoryboardOverlay> {
        match self.active_storyboard() {
            ActiveStoryboard::Intro => self.intro_storyboard.as_ref().filter(|s| !s.is_done()),
            ActiveStoryboard::Ending => self.ending_storyboard.as_ref().filter(|s| !s.is_done()),
            ActiveStoryboard::None => None,
        }
    }
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
/// - `<p1.def> <p2.def> <stage.def>` → as above, over that stage's background.
/// - `<file.sff> <file.air>` → legacy animation viewer.
/// - `<file.sff>` → legacy static sprite.
/// - anything missing/unloadable → falls back to the test pattern (no panic).
///
/// An optional 4th `.def` argument is taken as the stage; a missing/unloadable
/// stage degrades to the flat clear-color background, never failing the match.
///
/// `args` are the **positional** args with the `--p1-pal`/`--p2-pal` flags
/// already stripped (see [`parse_pal_flags`]); `pal` carries those per-player
/// `.act` palette selections, applied to the two fighters in the match modes.
fn select_mode(args: &[String], pal: PalSelection, renderer: &Renderer) -> Mode {
    match args.len() {
        // <p1.def> <p2.def> [stage.def] → two-player match from two characters.
        n if n >= 3 && is_def_path(&args[1]) && is_def_path(&args[2]) => {
            let stage = stage_arg(args, 3);
            load_match_or_fallback(Path::new(&args[1]), Path::new(&args[2]), stage, pal, renderer)
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
        // <p1.def> [stage.def] → two-player match, the same character both sides.
        n if n >= 2 && is_def_path(&args[1]) => {
            let def = Path::new(&args[1]);
            let stage = stage_arg(args, 2);
            load_match_or_fallback(def, def, stage, pal, renderer)
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
                // No default stage ships (clean-room / asset-blocked), so the
                // default match renders over the flat clear color.
                load_match_or_fallback(&def, &def, None, pal, renderer)
            } else {
                tracing::info!("No files and no default character; showing test pattern");
                tracing::info!("Usage: fp-app [p1.def [p2.def]] | <file.sff> [file.air]");
                let (s, p) = generate_test_pattern(renderer);
                Mode::TestPattern(s, p)
            }
        }
    }
}

/// Returns the `i`th CLI argument as a stage `.def` path, if present and ending
/// in `.def`. A non-`.def` extra argument is ignored (the stage stays absent),
/// which keeps the flat-background fallback.
fn stage_arg(args: &[String], i: usize) -> Option<&Path> {
    args.get(i)
        .filter(|a| is_def_path(a))
        .map(|a| Path::new(a.as_str()))
}

/// Builds a two-player [`Match`] from two `.def` paths, optionally over a stage,
/// falling back to the test pattern on a character-load failure (so a bad/missing
/// character never crashes the app). A bad/missing `stage_def` only drops the
/// background to the flat clear color — the match still runs.
/// The shipped clean-room default stage background, drawn behind the fighters
/// when no MUGEN `[BGdef]` stage is loaded.
const DEFAULT_STAGE_BG: &str = "assets/stages/dojo/bg.png";

/// Loads a PNG file as a full-color [`fp_render::ImageTexture`] (decoded to RGBA),
/// or `None` — warn/info-logged, never a panic — when the file is absent or can't
/// be decoded, so the match simply falls back to the flat clear color.
fn load_background_image(path: &str, renderer: &Renderer) -> Option<fp_render::ImageTexture> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            tracing::info!("no background image at {path}: {e}; using flat clear color");
            return None;
        }
    };
    let (rgba, w, h) = decode_png_rgba(&bytes).or_else(|| {
        tracing::warn!("background image {path} failed to decode; using flat clear color");
        None
    })?;
    tracing::info!("loaded stage background {path} ({w}x{h})");
    Some(fp_render::ImageTexture::new(
        renderer.device(),
        renderer.queue(),
        w,
        h,
        &rgba,
    ))
}

/// Decodes 8-bit PNG bytes into `(rgba, width, height)`, expanding palette /
/// grayscale and adding an opaque alpha channel as needed. Returns `None`
/// (never panics) for a malformed PNG or an unsupported 16-bit depth.
fn decode_png_rgba(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let mut decoder = png::Decoder::new(bytes);
    decoder.set_transformations(png::Transformations::EXPAND);
    let mut reader = decoder.read_info().ok()?;
    // Reject 16-bit-per-channel PNGs up front: `EXPAND` only widens *up* to 8-bit,
    // never down from 16, so decoding one would yield a double-width buffer our
    // 8-bit RGBA conversion can't interpret. Bail before allocating + decoding.
    if reader.info().bit_depth != png::BitDepth::Eight {
        return None;
    }
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    let rgba: Vec<u8> = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::GrayscaleAlpha => buf
            .chunks_exact(2)
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Indexed => return None,
    };
    Some((rgba, info.width, info.height))
}

fn load_match_or_fallback(
    p1_def: &Path,
    p2_def: &Path,
    stage_def: Option<&Path>,
    pal: PalSelection,
    renderer: &Renderer,
) -> Mode {
    match build_two_player_match(p1_def, p2_def, pal) {
        Ok(mut m) => {
            // The shipped common-effects (`fightfx`) set is loaded best-effort
            // (audit #17): when present, its AIR is installed on the match so
            // common (`fightfx`) hit-sparks spawn, and its SFF render bundle is
            // kept for drawing them; absent/bad, common sparks simply don't render
            // (no panic, no regression).
            let common_fx = match load_common_fx() {
                Some((air, render)) => {
                    m.set_common_fx(air);
                    Some(render)
                }
                None => None,
            };
            // Loading the stage is best-effort: `None` (no path, bad parse, or
            // missing SFF) keeps today's flat clear color (no regression).
            let stage = stage_def.and_then(StageRender::load);
            // With no MUGEN stage loaded, fall back to the shipped clean-room dojo
            // background image so the match renders over a real backdrop instead of
            // the flat grey clear color. A loaded stage draws its own backgrounds,
            // so the image is skipped there. Best-effort: a missing/bad asset just
            // leaves the flat clear color (no panic, no regression).
            let background = if stage.is_none() {
                load_background_image(DEFAULT_STAGE_BG, renderer)
            } else {
                None
            };
            // The screenpack HUD is best-effort too: `None` (no `fight.def` found,
            // bad parse, or missing `fight.sff`) falls back to the hand-rolled quad
            // HUD, so the default match looks exactly as before (no regression).
            let screenpack = load_screenpack(p1_def, renderer);
            // Intro/ending storyboards (audit #32) are P1's own declarations,
            // loaded best-effort: a character without them (or with an unloadable
            // SFF) simply plays no overlay — the normal intro/match is unchanged.
            let intro_storyboard = load_character_storyboard(p1_def, StoryboardKind::Intro, renderer);
            let ending_storyboard =
                load_character_storyboard(p1_def, StoryboardKind::Ending, renderer);
            Mode::Match(Box::new(MatchRun {
                m: Box::new(m),
                p1_render: FighterRender::default(),
                p2_render: FighterRender::default(),
                // The default constructor opens a real device when present and
                // falls back to a silent NullBackend otherwise — it never panics,
                // so the app runs identically with or without audio hardware.
                audio: AudioSystem::default(),
                p1_audio: FighterAudio::default(),
                p2_audio: FighterAudio::default(),
                stage,
                screenpack,
                intro_storyboard,
                ending_storyboard,
                intro_storyboard_done: false,
                common_fx,
                background,
            }))
        }
        Err(e) => {
            tracing::warn!("match failed to load: {e}; showing test pattern");
            let (s, p) = generate_test_pattern(renderer);
            Mode::TestPattern(s, p)
        }
    }
}

/// Finds and loads a `fight.def` screenpack into a GPU-resident
/// [`ScreenpackHud`], or returns `None` to fall back to the quad [`Hud`].
///
/// Search order (first hit wins), all best-effort:
/// 1. the `FP_SCREENPACK` environment variable, if it points at a `fight.def`;
/// 2. a `fight.def` next to the P1 character `.def`;
/// 3. a `data/fight.def` next to the P1 character `.def`.
///
/// No screenpack ships with the engine (clean-room / asset-blocked), so in the
/// default match this returns `None` and the hand-rolled quad HUD is used — no
/// regression. A found-but-unparseable `fight.def`, or one whose `fight.sff`
/// fails to load, also returns `None` (logged), never a panic.
fn load_screenpack(p1_def: &Path, renderer: &Renderer) -> Option<ScreenpackHud> {
    let fight_def = locate_fight_def(p1_def)?;
    tracing::info!("Loading screenpack: {}", fight_def.display());

    let def = match fp_formats::def::DefFile::load(&fight_def) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("screenpack {} failed to parse: {e}; using quad HUD", fight_def.display());
            return None;
        }
    };
    let layout = ScreenpackLayout::parse(&def);
    if layout.sff.is_empty() {
        tracing::warn!("screenpack {} has no [Files] sff; using quad HUD", fight_def.display());
        return None;
    }

    // Resolve and load the fight.sff relative to the fight.def directory.
    let sff_path = fp_formats::def::DefFile::resolve_path(&fight_def, &layout.sff);
    let sff = match SffFile::load(&sff_path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("screenpack sff {} failed to load: {e}; using quad HUD", sff_path.display());
            return None;
        }
    };

    // Load each font slot relative to the fight.def directory; a missing/bad font
    // becomes `None` (its text is skipped) rather than failing the whole HUD.
    let fonts = layout
        .fonts
        .iter()
        .map(|rel| {
            let path = fp_formats::def::DefFile::resolve_path(&fight_def, rel);
            match fp_formats::fnt::FntFont::load(&path) {
                Ok(f) => Some(f),
                Err(e) => {
                    tracing::warn!("screenpack font {} failed to load: {e}; skipping", path.display());
                    None
                }
            }
        })
        .collect();

    Some(ScreenpackHud::build(renderer, layout, &sff, fonts))
}

/// Returns the first existing `fight.def` candidate for the given P1 character
/// `.def`, or `None` if none is found. See [`load_screenpack`] for the search
/// order.
fn locate_fight_def(p1_def: &Path) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(env) = std::env::var("FP_SCREENPACK") {
        if !env.is_empty() {
            candidates.push(PathBuf::from(env));
        }
    }
    if let Some(dir) = p1_def.parent() {
        candidates.push(dir.join("fight.def"));
        candidates.push(dir.join("data").join("fight.def"));
    }
    candidates.into_iter().find(|p| p.exists())
}

/// Builds the per-frame [`MatchHudState`] the screenpack renderer draws from a
/// live [`Match`]: life/power fractions (via [`life_fraction`]/[`power_fraction`],
/// so the screenpack and quad HUDs agree), both fighter names, the timer, and the
/// round/KO/winner readout.
fn match_hud_state(m: &Match) -> MatchHudState {
    MatchHudState {
        p1_life: life_fraction(m.p1().life(), m.p1().life_max()),
        p2_life: life_fraction(m.p2().life(), m.p2().life_max()),
        p1_power: power_fraction(m.p1().power(), m.p1().power_max()),
        p2_power: power_fraction(m.p2().power(), m.p2().power_max()),
        p1_name: m.p1().loaded.name.clone(),
        p2_name: m.p2().loaded.name.clone(),
        // `Match::timer()` is FRAMES remaining (its doc: "Divide by 60 for
        // seconds"); `MatchHudState.timer_seconds` is whole seconds, drawn raw.
        // Convert here so a real fight.def clock reads e.g. 99, not 5940.
        timer_seconds: Some(timer_frames_to_seconds(m.timer())),
        round_text: round_readout(m),
        combo_count: active_combo_count(m),
    }
}

/// The hit count of the currently active combo for the screenpack combo counter.
///
/// MUGEN tracks the running combo on the *defender* (`GetHitVar(hitcount)`), so a
/// side's combo length is read off the opponent it is hitting. We surface
/// whichever side is comboing harder (the max of the two), which the renderer
/// then only draws once it reaches 2 (see [`fp_ui::combo_text`]).
fn active_combo_count(m: &Match) -> i32 {
    m.p1()
        .character
        .get_hit_vars
        .hitcount
        .max(m.p2().character.get_hit_vars.hitcount)
}

/// Converts the engine's frame-based round clock ([`Match::timer`], "frames
/// remaining") into the whole seconds that [`MatchHudState::timer_seconds`]
/// expects and the screenpack renderer draws verbatim.
///
/// The engine ticks at a fixed 60 Hz, so this is integer-divide by 60 (it floors,
/// matching MUGEN's whole-second countdown). Negative inputs are clamped to `0`.
fn timer_frames_to_seconds(frames: i32) -> i32 {
    (frames / 60).max(0)
}

/// The round-announcer text for the current match state: a `KO`/win/draw readout
/// once the round is decided, the round number during the intro, else empty.
///
/// Delegates to the pure [`round_label`] so the production mapping is exactly
/// what the unit test exercises (no duplicated `match`).
fn round_readout(m: &Match) -> String {
    round_label(m.round_state(), m.winner(), m.round_number())
}

/// The pure `(state, winner, round_number)` → announcer-text mapping behind
/// [`round_readout`], split out so it is testable without a live [`Match`].
///
/// `round_number` is only consulted for the [`RoundState::Intro`] case (the
/// `"ROUND N"` readout); the others ignore it.
fn round_label(state: RoundState, winner: Option<Winner>, round_number: i32) -> String {
    match (state, winner) {
        (RoundState::Ko, _) => "KO".to_string(),
        (RoundState::Win, Some(Winner::P1)) => "P1 WINS".to_string(),
        (RoundState::Win, Some(Winner::P2)) => "P2 WINS".to_string(),
        (RoundState::Win, _) => "DRAW".to_string(),
        (RoundState::Intro, _) => format!("ROUND {round_number}"),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// In-app screen state machine (Menu-2): Title -> Select -> Fight -> Title
// ---------------------------------------------------------------------------
//
// The pure menu/cursor/transition logic lives in `screens` (headless, unit-
// tested). This section is the GPU/SDL glue: it loads the motif (`system.def` +
// `select.def`) and the menu font once, owns the live [`RunScreen`], samples one
// rising-edge menu input per frame, and renders the Title/Select screens as text
// over a solid background. The Fight screen reuses the existing [`MatchRun`]
// render/HUD path unchanged.

/// The loaded default motif content the menu flow draws from: the parsed
/// `system.def` (title menu + select geometry), the parsed `select.def` roster,
/// and the `select.def`'s own path (so a roster `.def` resolves relative to it).
///
/// Every field has a sensible fallback: a missing/unparseable `system.def`
/// yields [`SystemDef::default`] (the title menu then uses its built-in
/// fallback), and a missing/unparseable roster yields an empty [`SelectDef`].
struct Motif {
    /// The parsed motif `system.def`.
    system: SystemDef,
    /// The parsed roster `select.def`.
    select: SelectDef,
    /// The on-disk path of the `select.def`, used to resolve roster `.def`s.
    select_path: PathBuf,
}

impl Motif {
    /// Loads the default motif best-effort from [`DEFAULT_SYSTEM_DEF`].
    ///
    /// Reads `system.def` (degrading to [`SystemDef::default`] when absent/bad),
    /// resolves its `[Files] select` relative to the `system.def` (falling back to
    /// the shipped [`DEFAULT_SELECT_DEF`] when it declares none or the file is
    /// missing), and parses the roster (degrading to an empty roster on a read
    /// error). Never panics — every failure logs and substitutes a default.
    fn load_default() -> Self {
        Self::load_from(Path::new(DEFAULT_SYSTEM_DEF), Path::new(DEFAULT_SELECT_DEF))
    }

    /// Loads a motif from an explicit `system.def` path, falling back to
    /// `fallback_select` when the motif declares/has no usable `select.def`. Split
    /// from [`Motif::load_default`] so the resolution logic is unit-testable by
    /// absolute path. Never panics.
    fn load_from(system_path: &Path, fallback_select: &Path) -> Self {
        let system = match fp_formats::def::DefFile::load(system_path) {
            Ok(def) => {
                tracing::info!("Loaded motif system.def: {}", system_path.display());
                SystemDef::parse(&def)
            }
            Err(e) => {
                tracing::warn!(
                    "motif {} not loaded ({e}); using built-in fallback menu",
                    system_path.display()
                );
                SystemDef::default()
            }
        };

        // Resolve the roster path: the motif's `[Files] select` relative to the
        // system.def, else the shipped fallback select.def.
        let select_path = resolve_select_path(system_path, &system, fallback_select);
        let select = match std::fs::read_to_string(&select_path) {
            Ok(text) => {
                tracing::info!("Loaded roster select.def: {}", select_path.display());
                SelectDef::parse(&text)
            }
            Err(e) => {
                tracing::warn!(
                    "roster {} not loaded ({e}); using empty roster",
                    select_path.display()
                );
                SelectDef::default()
            }
        };

        Self {
            system,
            select,
            select_path,
        }
    }
}

/// Resolves the roster `select.def` path for a motif: the motif's `[Files]
/// select` resolved relative to the `system.def` directory if it names one and
/// that file exists, else the shipped `fallback` path. Pure given the filesystem;
/// split out so the fallback logic is unit-testable.
fn resolve_select_path(system_path: &Path, system: &SystemDef, fallback: &Path) -> PathBuf {
    let declared = system.select_file.trim();
    if !declared.is_empty() {
        let resolved = fp_formats::def::DefFile::resolve_path(system_path, declared);
        if resolved.exists() {
            return resolved;
        }
        tracing::warn!(
            "motif declares select = {declared} but {} is missing; using fallback roster",
            resolved.display()
        );
    }
    fallback.to_path_buf()
}

/// The live in-app screen. The menu flow starts in [`RunScreen::Title`] and
/// transitions Title -> Select -> Fight -> Title; [`RunScreen::Quit`] ends the
/// loop. The Fight variant carries the same [`MatchRun`] the direct-CLI match
/// path uses, so the fight renders identically.
enum RunScreen {
    /// The title-screen main menu.
    Title(screens::TitleMenu),
    /// The character-select grid.
    Select(screens::SelectScreen),
    /// A running two-player match. On match-over the flow returns to Title.
    Fight(Box<MatchRun>),
    /// Leave the application.
    Quit,
}

/// The whole menu-mode runtime: the loaded motif, the menu font, and the live
/// screen. Owns the transitions between Title/Select/Fight.
struct MenuApp {
    /// The loaded default motif (title menu + roster).
    motif: Motif,
    /// The bitmap font used to draw the text menus (the shipped HUD font).
    /// `None` when the font is absent — the menus then draw nothing but the flow
    /// still works (and logs), so a missing font never traps the app.
    font: Option<GlyphFont>,
    /// The current screen.
    screen: RunScreen,
}

impl MenuApp {
    /// Builds the menu runtime: loads the default motif + menu font and starts on
    /// the Title screen built from the motif (or its built-in fallback).
    fn new(renderer: &Renderer) -> Self {
        let motif = Motif::load_default();
        let title = screens::TitleMenu::from_system(&motif.system);
        Self {
            motif,
            font: load_hud_font(renderer),
            screen: RunScreen::Title(title),
        }
    }

    /// Whether the app should keep running (false once the menu requested Quit).
    fn running(&self) -> bool {
        !matches!(self.screen, RunScreen::Quit)
    }

    /// Enters the character-select screen for the given mode.
    fn enter_select(&mut self, mode: screens::SelectMode) {
        let screen = screens::SelectScreen::new(
            mode,
            &self.motif.select,
            &self.motif.system.select_info,
            &self.motif.select_path,
        );
        if screen.is_empty() {
            tracing::warn!("roster has no choosable characters; returning to title");
            self.screen = RunScreen::Title(screens::TitleMenu::from_system(&self.motif.system));
        } else {
            self.screen = RunScreen::Select(screen);
        }
    }

    /// Builds the two-player match for a completed [`screens::MatchPick`] and
    /// enters the Fight screen, or returns to Title on a load failure (so a bad
    /// roster `.def` never crashes the flow).
    fn enter_fight(&mut self, pick: screens::MatchPick, renderer: &Renderer) {
        tracing::info!(
            "Starting match: P1={} ({}) vs P2={} ({})",
            pick.p1_name,
            pick.p1_def.display(),
            pick.p2_name,
            pick.p2_def.display(),
        );
        match build_match_run(&pick.p1_def, &pick.p2_def, renderer) {
            Some(run) => self.screen = RunScreen::Fight(Box::new(run)),
            None => {
                tracing::warn!("could not start match; returning to title");
                self.screen =
                    RunScreen::Title(screens::TitleMenu::from_system(&self.motif.system));
            }
        }
    }

    /// Returns to the Title screen (fresh cursor).
    fn return_to_title(&mut self) {
        self.screen = RunScreen::Title(screens::TitleMenu::from_system(&self.motif.system));
    }
}

/// Builds a [`MatchRun`] (match + per-run render/audio/HUD resources) for two
/// character `.def`s, mirroring the direct-CLI match path but without a stage or
/// screenpack search (the menu flow ships none). Returns `None` on a character
/// load failure so the caller can fall back to the title menu. Never panics.
fn build_match_run(p1_def: &Path, p2_def: &Path, renderer: &Renderer) -> Option<MatchRun> {
    match load_match_or_fallback(p1_def, p2_def, None, PalSelection::default(), renderer) {
        Mode::Match(run) => Some(*run),
        // A character that fails to load degrades to the test pattern in
        // `load_match_or_fallback`; the menu flow treats that as "couldn't start"
        // and returns to the title rather than showing a checkerboard.
        _ => None,
    }
}

/// Draws the Title screen: the motif name (when present) and each menu item as a
/// line of text, the highlighted item prefixed with a cursor marker, over the
/// already-cleared solid background. A no-op when no font is loaded.
fn draw_title_screen(
    frame: &mut fp_render::RenderFrame<'_>,
    font: &GlyphFont,
    menu: &screens::TitleMenu,
    title_name: &str,
    win_w: f32,
) {
    /// Menu text scale (the ~7px font at 3x ≈ 21px lines).
    const SCALE: f32 = 3.0;
    /// Title-name scale, a touch larger than the items.
    const TITLE_SCALE: f32 = 4.0;
    let line_h = font.line_height() as f32 * SCALE;

    // Motif name as a header near the top.
    let mut y = 60.0;
    if !title_name.is_empty() {
        draw_centered_text(frame, font, &to_menu_text(title_name), win_w, y, TITLE_SCALE, 1.0);
    }

    // Menu items, vertically stacked and centered, the cursor item marked.
    y = 180.0;
    for (i, entry) in menu.entries.iter().enumerate() {
        let selected = i == menu.cursor;
        // A leading "> " marks the highlighted item; others are indented to match
        // so the labels line up. The font is uppercase-only, so upcase the label.
        let line = if selected {
            format!("> {}", to_menu_text(&entry.label))
        } else {
            format!("  {}", to_menu_text(&entry.label))
        };
        let alpha = if selected { 1.0 } else { 0.6 };
        draw_centered_text(frame, font, &line, win_w, y, SCALE, alpha);
        y += line_h + 8.0;
    }
}

/// Draws the character-select screen: a header, the roster as a centered text
/// list (one cell per line) with the P1 (and, in Versus, P2) cursor markers, and
/// each player's locked-in pick. A no-op when no font is loaded.
fn draw_select_screen(
    frame: &mut fp_render::RenderFrame<'_>,
    font: &GlyphFont,
    screen: &screens::SelectScreen,
    win_w: f32,
) {
    /// Roster text scale.
    const SCALE: f32 = 2.5;
    let line_h = font.line_height() as f32 * SCALE;

    draw_centered_text(frame, font, "SELECT", win_w, 40.0, 3.0, 1.0);

    let mut y = 120.0;
    for (i, cell) in screen.cells.iter().enumerate() {
        let name = match cell {
            screens::RosterCell::Character(e) => to_menu_text(&e.name),
            screens::RosterCell::Random => "RANDOM".to_string(),
        };
        // Cursor markers: P1 marks its cell, P2 marks its own (Versus only). When
        // both land on the same cell show a combined marker.
        let p1_here = !screen.is_empty() && screen.p1_cursor == i;
        let p2_here = screen.mode == screens::SelectMode::Versus && screen.p2_cursor == i;
        let marker = match (p1_here, p2_here) {
            (true, true) => "P12",
            (true, false) => "P1 ",
            (false, true) => "P2 ",
            (false, false) => "   ",
        };
        let locked = screen.p1_locked == Some(i) || screen.p2_locked == Some(i);
        let line = if locked {
            format!("{marker} {name} *")
        } else {
            format!("{marker} {name}")
        };
        let alpha = if p1_here || p2_here { 1.0 } else { 0.6 };
        draw_centered_text(frame, font, &line, win_w, y, SCALE, alpha);
        y += line_h + 6.0;
    }

    // A short hint at the bottom.
    let hint = match screen.mode {
        screens::SelectMode::Versus => "P1 THEN P2 PICK",
        screens::SelectMode::Training => "PICK FIGHTER",
    };
    draw_centered_text(frame, font, hint, win_w, y + 20.0, 2.0, 0.8);
}

/// Upcases `s` into the menu font's supported glyph set (the shipped FNT covers
/// `0-9 A-Z`, space, and colon). Lowercase becomes uppercase; any character the
/// font can't draw is harmlessly skipped by `draw_text`'s missing-glyph
/// fallback, so this only needs to fold case for readability.
fn to_menu_text(s: &str) -> String {
    s.to_ascii_uppercase()
}

/// Advances a [`MatchRun`] one 60Hz tick and plays the frame's surfaced sound
/// requests (P1 then P2). Factored out of the run loop so both the direct-CLI
/// match path and the menu Fight screen drive a match identically.
fn tick_match_run(run: &mut MatchRun, p1_input: MatchInput, p2_input: MatchInput) {
    run.m.tick(p1_input, p2_input);
    // AFTER the tick: play this frame's surfaced sound requests, P1 then P2, each
    // from its own decoded-sound cache. Graceful throughout — a silent backend or
    // a missing sound is a no-op.
    run.p1_audio
        .play_requests(&mut run.audio, run.m.p1(), run.m.p1_sound_requests());
    run.p2_audio
        .play_requests(&mut run.audio, run.m.p2(), run.m.p2_sound_requests());
}

/// Decodes everything a [`MatchRun`] needs to draw this frame (both fighters'
/// current sprites, live hit-spark sprites, stage background sprites, and the
/// active storyboard overlay) into the GPU caches. Must run BEFORE `begin_frame`
/// because decoding needs `&Renderer`, which a live `RenderFrame` holds borrowed.
/// Factored out of the run loop so the Fight screen caches identically.
fn cache_match_run(run: &mut MatchRun, renderer: &Renderer) {
    cache_player_sprite(&mut run.p1_render, run.m.p1(), renderer);
    cache_player_sprite(&mut run.p2_render, run.m.p2(), renderer);
    cache_effect_sprites(run, renderer);
    if let Some(stage) = run.stage.as_mut() {
        stage.cache_sprites(renderer);
    }
    run.tick_storyboard(renderer);
}

/// Draws a whole [`MatchRun`] frame: stage layers, both fighters (sprpriority
/// ordered), hit-sparks, the optional Clsn debug overlay, the HUD (screenpack or
/// quad), and any active storyboard overlay. Factored out of the run loop so the
/// menu Fight screen renders byte-identically to the direct-CLI match path.
fn draw_match_run(
    frame: &mut fp_render::RenderFrame<'_>,
    run: &MatchRun,
    hud: &Hud,
    overlay_enabled: bool,
    win_wf: f32,
    win_hf: f32,
) {
    // Camera follows the fighters' midpoint, clamped to the stage's horizontal
    // bounds. With no stage the camera stays at 0 (flat-background path).
    let camera_x = run
        .stage
        .as_ref()
        .map(|s| s.stage.camera_follow_x(run.m.p1().pos().x, run.m.p2().pos().x))
        .unwrap_or(0.0);

    // Full-color background image first of all (behind everything), when no MUGEN
    // stage is loaded. Scaled to fill the whole window.
    if let Some(bg) = run.background.as_ref() {
        frame.draw_image(bg, 0.0, 0.0, win_wf, win_hf);
    }

    // Back background layers first (behind the fighters).
    if let Some(stage) = run.stage.as_ref() {
        stage.draw_layer(frame, BgLayer::Back, camera_x, win_wf, win_hf);
    }

    // Draw both fighters ordered by sprite-draw priority (MUGEN `sprpriority`,
    // audit #16): the lower priority is drawn FIRST (behind), the higher OVER it.
    // A tie keeps P1 behind P2 (stable, deterministic order).
    if p1_draws_behind_p2(
        run.m.p1().character.cur_sprpriority,
        run.m.p2().character.cur_sprpriority,
    ) {
        draw_player(frame, &run.p1_render, run.m.p1(), camera_x, win_wf, win_hf);
        draw_player(frame, &run.p2_render, run.m.p2(), camera_x, win_wf, win_hf);
    } else {
        draw_player(frame, &run.p2_render, run.m.p2(), camera_x, win_wf, win_hf);
        draw_player(frame, &run.p1_render, run.m.p1(), camera_x, win_wf, win_hf);
    }

    // Hit-spark effects (audit #17), drawn OVER both fighters, under front BG/HUD.
    draw_effects(
        frame,
        EffectRenders {
            p1_render: &run.p1_render,
            p2_render: &run.p2_render,
            common_render: run.common_fx.as_ref().map(|c| &c.render),
        },
        &run.m,
        camera_x,
        win_wf,
        win_hf,
    );

    // Front background layers, over the fighters but under the HUD.
    if let Some(stage) = run.stage.as_ref() {
        stage.draw_layer(frame, BgLayer::Front, camera_x, win_wf, win_hf);
    }

    // Optional Clsn debug overlay (F1).
    if overlay_enabled {
        draw_player_clsn(frame, run.m.p1(), camera_x, win_wf, win_hf);
        draw_player_clsn(frame, run.m.p2(), camera_x, win_wf, win_hf);
    }

    // HUD on top: a loaded screenpack draws real lifebars/text, else the quad HUD.
    match run.screenpack.as_ref() {
        Some(screenpack) => {
            let state = match_hud_state(&run.m);
            screenpack.draw(frame, &state);
        }
        None => hud.draw(frame, win_wf, &run.m),
    }

    // Intro/ending storyboard overlay (audit #32), drawn LAST and only while one
    // is active.
    if let Some(overlay) = run.storyboard_to_draw() {
        overlay.draw(frame, win_wf, win_hf);
    }
}

fn run() -> fp_core::FpResult<()> {
    // --- SDL2 setup ---
    let sdl = sdl2::init().map_err(|e| fp_core::FpError::Other(format!("SDL2 init: {e}")))?;
    let video = sdl
        .video()
        .map_err(|e| fp_core::FpError::Other(format!("SDL2 video: {e}")))?;

    // Game-controller support is optional: if the subsystem can't initialize
    // (no driver / headless), we log and run keyboard-only — never a fatal error.
    let mut controllers = match sdl.game_controller() {
        Ok(subsystem) => Some(Controllers::new(subsystem)),
        Err(e) => {
            tracing::warn!("controller: subsystem unavailable, keyboard only: {e}");
            None
        }
    };

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
    // Strip the per-player `.act` palette flags (`--p1-pal N` / `--p2-pal N`,
    // FL2b) first; the remaining positional args drive file routing as before.
    let raw_args: Vec<String> = std::env::args().collect();
    let (pal, args) = parse_pal_flags(&raw_args);

    // The top-level launch route: no file args (or an explicit `menu`) launches
    // the in-app Title menu; any direct content path (p1.def/sff/...) keeps the
    // legacy direct view exactly as before. This REPLACES the old no-args default
    // (a two-KFM match) with the menu, so a fresh clean-room checkout boots into
    // the title screen over the shipped trainingdummy roster (no KFM needed).
    let route = cli_route(&args);
    let mut menu_app = match route {
        CliRoute::Menu => Some(MenuApp::new(&renderer)),
        CliRoute::Direct => None,
    };
    // The direct-CLI content mode, only built on the Direct route.
    let mut mode = match route {
        CliRoute::Direct => Some(select_mode(&args, pal, &renderer)),
        CliRoute::Menu => None,
    };

    // The minimal match HUD (life bars + KO marker). Built once; drawn in the
    // two-player match mode and the menu Fight screen.
    let hud = Hud::new(&renderer);

    // Edge-detection state for the text menus: last frame's held menu controls.
    // Updated once per real frame so a held key moves the cursor one cell.
    let mut prev_menu_held = screens::HeldMenuInput::default();
    // A monotonic frame counter, used as the deterministic-friendly RNG seed for
    // RandomSelect on the character-select screen.
    let mut frame_counter: u64 = 0;

    // --- Main loop ---
    let mut event_pump = sdl
        .event_pump()
        .map_err(|e| fp_core::FpError::Other(format!("SDL2 event pump: {e}")))?;

    let mut previous = Instant::now();
    let mut accumulator = Duration::ZERO;
    let mut running = true;
    // Clsn hitbox/hurtbox debug overlay (audit #34), toggled with F1. Off by
    // default; when on, both fighters' current-frame Clsn1 (red) and Clsn2
    // (blue) boxes are drawn over the sprites in the two-player match mode.
    let mut overlay_enabled = false;

    while running {
        // Per-frame edge flags driven from discrete key events (below).
        // `esc_pressed` doubles as the menu "back" in menu mode (back out a
        // screen) and a hard quit in direct mode; `confirm_pressed` (Enter/Space)
        // confirms a menu item. These are edges by construction (one KeyDown per
        // physical press), complementing the held-state directions sampled below.
        let mut esc_pressed = false;
        let mut confirm_key_pressed = false;
        // Poll events
        for event in event_pump.poll_iter() {
            match event {
                Event::Quit { .. } => {
                    // A window close is always a hard quit, in any mode.
                    running = false;
                }
                Event::KeyDown {
                    keycode: Some(Keycode::Escape),
                    repeat: false,
                    ..
                } => {
                    esc_pressed = true;
                }
                Event::KeyDown {
                    keycode: Some(Keycode::Return | Keycode::Space),
                    repeat: false,
                    ..
                } => {
                    confirm_key_pressed = true;
                }
                Event::KeyDown {
                    keycode: Some(Keycode::F1),
                    repeat: false,
                    ..
                } => {
                    overlay_enabled = !overlay_enabled;
                    tracing::info!(
                        "Clsn debug overlay {}",
                        if overlay_enabled { "ON" } else { "OFF" }
                    );
                }
                Event::Window {
                    win_event: sdl2::event::WindowEvent::Resized(w, h),
                    ..
                } => {
                    renderer.resize(w as u32, h as u32);
                }
                // Controller hotplug. `which` is a joystick *index* on add and an
                // instance *id* on remove (SDL convention). Both are handled
                // failure-tolerantly: a missing subsystem or a failed open is a
                // no-op, never a panic.
                Event::ControllerDeviceAdded { which, .. } => {
                    if let Some(c) = controllers.as_mut() {
                        c.on_device_added(which);
                    }
                }
                Event::ControllerDeviceRemoved { which, .. } => {
                    if let Some(c) = controllers.as_mut() {
                        c.on_device_removed(which);
                    }
                }
                _ => {}
            }
        }

        // Fixed timestep accumulation
        let current = Instant::now();
        accumulator += current - previous;
        previous = current;

        // Sample every physical input ONCE per real frame, BEFORE the
        // fixed-timestep catch-up loop (audit #27). On a frame that has to run
        // multiple ticks to catch up, every sub-tick is driven by this single
        // physical snapshot — re-reading `keyboard_state()` / the controller
        // inside the loop would replay the same live state N times anyway, but
        // doing it here makes the "one input per frame" semantics explicit and
        // keeps press-vs-hold edges and command timing correct (one buffer push
        // per frame). The snapshots are cheap, so we take them unconditionally
        // even in non-Match modes (which ignore them).
        //
        // P1 acts from the keyboard OR controller 0 (either source can assert any
        // bit). If a second controller is present, it drives P2 for a real
        // two-human match; otherwise P2 stays an idle dummy as before.
        let kbd_input = match_input_from_keyboard(&event_pump.keyboard_state());
        let pad0 = controllers
            .as_ref()
            .and_then(|c| c.input(0))
            .map(|c| controller_to_match_input(&c))
            .unwrap_or_else(MatchInput::none);
        let p1_input = merge_match_input(kbd_input, pad0);
        let p2_input = controllers
            .as_ref()
            .and_then(|c| c.input(1))
            .map(|c| controller_to_match_input(&c))
            .unwrap_or_else(MatchInput::none);

        frame_counter = frame_counter.wrapping_add(1);

        // --- Menu screen update (menu mode only) ---
        // Build the rising-edge menu input for this frame: directions from the
        // held P1 input (keyboard OR controller 0), confirm from Enter/Space OR a
        // controller face/attack button, back from Esc OR controller B. A held
        // direction yields exactly one cursor step thanks to edge detection.
        if let Some(app) = menu_app.as_mut() {
            // Held state this frame (for direction edges).
            let held = screens::HeldMenuInput {
                up: p1_input.up,
                down: p1_input.down,
                left: p1_input.left,
                right: p1_input.right,
                // A held confirm/back is also fine — edge detection makes it
                // one-shot. Map controller A (a) to confirm, B (b) to back.
                confirm: p1_input.a,
                back: p1_input.b,
            };
            let mut menu_in = screens::MenuInput::from_edges(held, prev_menu_held);
            // Fold in the discrete key-event edges (these are already one-shot).
            menu_in.confirm = menu_in.confirm || confirm_key_pressed;
            menu_in.back = menu_in.back || esc_pressed;
            prev_menu_held = held;

            // Drive the active menu screen. The Fight screen is driven by the
            // normal match tick path below (not here); Title/Select consume the
            // menu input and may transition.
            match app.screen {
                RunScreen::Title(ref mut menu) => {
                    if let Some(action) = menu.update(menu_in) {
                        match action {
                            screens::TitleAction::Select(mode) => app.enter_select(mode),
                            screens::TitleAction::Quit => app.screen = RunScreen::Quit,
                            screens::TitleAction::NoOp => {}
                        }
                    }
                }
                RunScreen::Select(ref mut select) => {
                    match select.update(menu_in, frame_counter) {
                        screens::SelectOutcome::Pending => {}
                        screens::SelectOutcome::Cancelled => app.return_to_title(),
                        screens::SelectOutcome::Done(pick) => app.enter_fight(pick, &renderer),
                    }
                }
                RunScreen::Fight(_) | RunScreen::Quit => {}
            }

            if !app.running() {
                running = false;
            }
        } else if esc_pressed {
            // Direct CLI modes keep the original Esc-quits behaviour.
            running = false;
        }

        // --- Fixed-timestep tick (catch-up loop) ---
        // Both the direct-CLI match and the menu Fight screen drive their match at
        // a fixed 60Hz here; the Title/Select menu screens are event-driven (no
        // per-tick simulation), so they only need a render below. Viewer/Static/
        // TestPattern tick as before.
        while accumulator >= TICK_DURATION {
            match mode.as_mut() {
                Some(Mode::Match(run)) => tick_match_run(run, p1_input, p2_input),
                Some(Mode::Viewer(v)) => v.tick(),
                Some(Mode::Static(..)) | Some(Mode::TestPattern(..)) | None => {}
            }
            // The menu Fight screen advances its match here too.
            if let Some(RunScreen::Fight(run)) =
                menu_app.as_mut().map(|a| &mut a.screen)
            {
                tick_match_run(run, p1_input, p2_input);
            }
            accumulator -= TICK_DURATION;
        }

        // After the catch-up loop: if the menu's match is over, return to the
        // title screen (Menu-2 deliverable 4). Done outside the tick loop so the
        // transition happens once per real frame, after all sub-ticks.
        if let Some(app) = menu_app.as_mut() {
            if let RunScreen::Fight(run) = &app.screen {
                if run.m.match_winner().is_some() {
                    tracing::info!("Match over; returning to title menu");
                    app.return_to_title();
                }
            }
        }

        // Ensure the current animation frame's sprite is cached before rendering.
        // Caching needs `&Renderer`, which a live `RenderFrame` would hold
        // borrowed, so it must happen before `begin_frame`.
        match mode.as_mut() {
            Some(Mode::Match(run)) => cache_match_run(run, &renderer),
            Some(Mode::Viewer(v)) => {
                if let Some(sid) = v.current_frame().map(|f| f.sprite) {
                    v.get_or_create_sprite(sid, &renderer);
                }
            }
            Some(Mode::Static(..)) | Some(Mode::TestPattern(..)) | None => {}
        }
        if let Some(RunScreen::Fight(run)) = menu_app.as_mut().map(|a| &mut a.screen) {
            cache_match_run(run, &renderer);
        }

        // Render
        let mut frame = renderer.begin_frame()?;
        frame.clear(0.1, 0.1, 0.15);

        let (win_w, win_h) = window.size();
        let win_wf = win_w as f32;
        let win_hf = win_h as f32;

        // Direct-CLI content modes render exactly as before.
        match mode.as_ref() {
            Some(Mode::Match(run)) => {
                draw_match_run(&mut frame, run, &hud, overlay_enabled, win_wf, win_hf);
            }
            Some(Mode::Viewer(v)) => {
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
            Some(Mode::Static(sprite_tex, palette_tex))
            | Some(Mode::TestPattern(sprite_tex, palette_tex)) => {
                let params = SpriteDrawParams {
                    x: (win_w as f32 - sprite_tex.width as f32) / 2.0,
                    y: (win_h as f32 - sprite_tex.height as f32) / 2.0,
                    ..Default::default()
                };
                frame.draw_sprite(sprite_tex, palette_tex, &params);
            }
            None => {}
        }

        // Menu-mode screens render over the solid clear color: Title/Select as
        // text, Fight via the shared match draw path (identical to the direct
        // match render above).
        if let Some(app) = menu_app.as_ref() {
            match &app.screen {
                RunScreen::Title(menu) => {
                    if let Some(font) = app.font.as_ref() {
                        draw_title_screen(&mut frame, font, menu, &app.motif.system.name, win_wf);
                    }
                }
                RunScreen::Select(select) => {
                    if let Some(font) = app.font.as_ref() {
                        draw_select_screen(&mut frame, font, select, win_wf);
                    }
                }
                RunScreen::Fight(run) => {
                    draw_match_run(&mut frame, run, &hud, overlay_enabled, win_wf, win_hf);
                }
                RunScreen::Quit => {}
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
    fn controller_to_match_input_carries_directions_and_buttons() {
        // A right-down + (a, c) controller snapshot must surface those exact bits.
        let raw = RawController {
            dpad_right: true,
            stick_y: 30000, // down (SDL: +Y is down)
            face_west: true,     // a
            shoulder_right: true, // c
            ..RawController::default()
        };
        let ci = map_controller(&raw, DEADZONE_DEFAULT);
        let mi = controller_to_match_input(&ci);
        assert!(mi.right);
        assert!(mi.down);
        assert!(!mi.up);
        assert!(!mi.left);
        assert!(mi.a);
        assert!(mi.c);
        assert!(!mi.b && !mi.x && !mi.y && !mi.z);
    }

    #[test]
    fn controller_to_match_input_neutral_is_none() {
        let ci = map_controller(&RawController::default(), DEADZONE_DEFAULT);
        assert_eq!(controller_to_match_input(&ci), MatchInput::none());
    }

    #[test]
    fn merge_match_input_is_per_field_or() {
        let kbd = MatchInput {
            left: true,
            a: true,
            ..MatchInput::none()
        };
        let pad = MatchInput {
            right: true,
            a: true,
            z: true,
            ..MatchInput::none()
        };
        let merged = merge_match_input(kbd, pad);
        assert!(merged.left, "from keyboard");
        assert!(merged.right, "from controller");
        assert!(merged.a, "asserted by both");
        assert!(merged.z, "from controller");
        assert!(!merged.up && !merged.down);
        assert!(!merged.b && !merged.c && !merged.x && !merged.y);
    }

    #[test]
    fn merge_with_none_is_identity() {
        let only_kbd = MatchInput {
            up: true,
            x: true,
            ..MatchInput::none()
        };
        assert_eq!(merge_match_input(only_kbd, MatchInput::none()), only_kbd);
        assert_eq!(merge_match_input(MatchInput::none(), only_kbd), only_kbd);
    }

    // -----------------------------------------------------------------------
    // Keyboard key map (T024): the player-1 scancode -> engine-input path.
    //
    // `match_input_from_held` is the pure core of `match_input_from_keyboard`;
    // it takes a held-key oracle so the key map is testable without a live SDL
    // context. Tests build a synthetic "held" set and assert every documented
    // direction and attack button reaches the right `MatchInput` bit, that WASD
    // and the arrow keys alias the same direction, and that directions stay
    // absolute (never pre-rotated by facing). The MatchInput -> engine raw
    // InputState hop is covered in `fp-input` (see `playability_tests`).
    // -----------------------------------------------------------------------

    /// Builds the `MatchInput` for a set of held scancodes via the pure key map.
    fn kbd(held: &[Scancode]) -> MatchInput {
        match_input_from_held(|sc| held.contains(&sc))
    }

    #[test]
    fn keyboard_no_keys_held_is_none() {
        assert_eq!(kbd(&[]), MatchInput::none());
    }

    #[test]
    fn keyboard_wasd_drives_each_direction() {
        assert!(kbd(&[Scancode::A]).left, "A -> left");
        assert!(kbd(&[Scancode::D]).right, "D -> right");
        assert!(kbd(&[Scancode::W]).up, "W -> up (jump)");
        assert!(kbd(&[Scancode::S]).down, "S -> down (crouch)");
        // Each is exactly one bit, nothing else.
        let left = kbd(&[Scancode::A]);
        assert_eq!(
            left,
            MatchInput {
                left: true,
                ..MatchInput::none()
            }
        );
    }

    #[test]
    fn keyboard_arrow_keys_drive_each_direction() {
        assert!(kbd(&[Scancode::Left]).left, "Left arrow -> left");
        assert!(kbd(&[Scancode::Right]).right, "Right arrow -> right");
        assert!(kbd(&[Scancode::Up]).up, "Up arrow -> up (jump)");
        assert!(kbd(&[Scancode::Down]).down, "Down arrow -> down (crouch)");
    }

    #[test]
    fn keyboard_wasd_and_arrows_alias_same_direction() {
        // Either source asserts the same bit, and holding both is just that bit.
        assert_eq!(kbd(&[Scancode::A]), kbd(&[Scancode::Left]));
        assert_eq!(kbd(&[Scancode::A, Scancode::Left]), kbd(&[Scancode::A]));
    }

    #[test]
    fn keyboard_each_attack_button_maps_to_documented_bit() {
        // Punch row a/b/c on U/I/O.
        assert!(kbd(&[Scancode::U]).a, "U -> a");
        assert!(kbd(&[Scancode::I]).b, "I -> b");
        assert!(kbd(&[Scancode::O]).c, "O -> c");
        // Kick row x/y/z on J/K/L.
        assert!(kbd(&[Scancode::J]).x, "J -> x");
        assert!(kbd(&[Scancode::K]).y, "K -> y");
        assert!(kbd(&[Scancode::L]).z, "L -> z");

        // Each attack key sets exactly its own bit and no other.
        assert_eq!(
            kbd(&[Scancode::U]),
            MatchInput {
                a: true,
                ..MatchInput::none()
            }
        );
    }

    #[test]
    fn keyboard_combined_walk_and_attack() {
        // Holding D + U (walk right + light punch) sets exactly those two bits,
        // the common "attack while advancing" case.
        let mi = kbd(&[Scancode::D, Scancode::U]);
        assert_eq!(
            mi,
            MatchInput {
                right: true,
                a: true,
                ..MatchInput::none()
            }
        );
    }

    #[test]
    fn keyboard_bindings_cover_every_input_field() {
        // The binding table must reach all four directions and all six attack
        // buttons, so no documented input is unreachable from the keyboard.
        let all: Vec<InputField> = KEYBOARD_BINDINGS.iter().map(|&(_, f)| f).collect();
        for field in [
            InputField::Up,
            InputField::Down,
            InputField::Left,
            InputField::Right,
            InputField::A,
            InputField::B,
            InputField::C,
            InputField::X,
            InputField::Y,
            InputField::Z,
        ] {
            assert!(
                all.contains(&field),
                "no keyboard binding produces {field:?}"
            );
        }
    }

    #[test]
    fn keyboard_directions_are_absolute_not_prerotated() {
        // The keyboard map emits ABSOLUTE screen directions; left and right are
        // never swapped by the app (facing is resolved later inside the engine's
        // CommandMatcher). Holding A must always be `left`, never `right`,
        // regardless of which way the (eventual) fighter faces.
        let l = kbd(&[Scancode::A]);
        assert!(l.left && !l.right);
        let r = kbd(&[Scancode::D]);
        assert!(r.right && !r.left);
    }

    #[test]
    fn is_def_path_detects_def() {
        assert!(is_def_path("kfm.def"));
        assert!(is_def_path("path/to/KFM.DEF"));
        assert!(!is_def_path("kfm.sff"));
        assert!(!is_def_path("kfm"));
    }

    // -----------------------------------------------------------------------
    // Intro / ending storyboard overlay (audit #32)
    // -----------------------------------------------------------------------

    #[test]
    fn storyboard_kind_def_keys() {
        assert_eq!(StoryboardKind::Intro.def_key(), "intro.storyboard");
        assert_eq!(StoryboardKind::Ending.def_key(), "ending.storyboard");
    }

    /// The intro overlay is active only during round 1's [`RoundState::Intro`],
    /// while an intro is loaded and not yet finished.
    #[test]
    fn intro_storyboard_active_only_in_round1_intro() {
        // Round 1 intro, intro loaded, not done -> Intro.
        assert_eq!(
            active_storyboard(RoundState::Intro, 1, false, false, true, false),
            ActiveStoryboard::Intro
        );
        // Past the intro phase (Fight) -> nothing.
        assert_eq!(
            active_storyboard(RoundState::Fight, 1, false, false, true, false),
            ActiveStoryboard::None
        );
        // Later round's intro -> nothing (intro is a once-per-match cutscene).
        assert_eq!(
            active_storyboard(RoundState::Intro, 2, false, false, true, false),
            ActiveStoryboard::None
        );
        // Intro already finished -> nothing (don't loop it).
        assert_eq!(
            active_storyboard(RoundState::Intro, 1, false, true, true, false),
            ActiveStoryboard::None
        );
    }

    /// With no intro storyboard loaded, the intro gate never fires — the normal
    /// intro plays with no overlay (no regression).
    #[test]
    fn no_intro_storyboard_no_overlay() {
        assert_eq!(
            active_storyboard(RoundState::Intro, 1, false, false, false, false),
            ActiveStoryboard::None
        );
    }

    /// The ending overlay is active once the match is decided, taking precedence,
    /// and only when an ending is loaded.
    #[test]
    fn ending_storyboard_active_when_match_over() {
        // Match over, ending loaded -> Ending.
        assert_eq!(
            active_storyboard(RoundState::Win, 3, true, true, true, true),
            ActiveStoryboard::Ending
        );
        // Match over but no ending loaded -> nothing (normal match-over view).
        assert_eq!(
            active_storyboard(RoundState::Win, 3, true, true, true, false),
            ActiveStoryboard::None
        );
        // Match NOT over -> not the ending, even if loaded.
        assert_eq!(
            active_storyboard(RoundState::Fight, 1, false, false, true, true),
            ActiveStoryboard::None
        );
    }

    /// During an in-progress match (not over, past the intro) no overlay is ever
    /// active even when both are loaded — the live fight is untouched.
    #[test]
    fn mid_fight_has_no_overlay() {
        assert_eq!(
            active_storyboard(RoundState::Fight, 1, false, true, true, true),
            ActiveStoryboard::None
        );
        assert_eq!(
            active_storyboard(RoundState::Ko, 1, false, true, true, true),
            ActiveStoryboard::None
        );
    }

    /// The overlay loader no-ops (returns `None`) for a character `.def` that
    /// declares no `intro.storyboard`/`ending.storyboard` key — gated as required,
    /// without needing a GPU. (We read the key via the same DEF path the loader
    /// uses; a renderer is only needed once a storyboard is actually found.)
    #[test]
    fn character_without_storyboard_key_yields_none() {
        // A minimal in-memory character def with no storyboard declarations.
        let def_text = "[Info]\nname = \"X\"\n[Files]\nsprite = x.sff\n";
        let def = fp_formats::def::DefFile::from_str(def_text).expect("parse def");
        // The loader's gating predicate: the key must be present and non-empty.
        let has_intro = def
            .get(STORYBOARD_SECTION, StoryboardKind::Intro.def_key())
            .map(str::trim)
            .is_some_and(|s| !s.is_empty());
        let has_ending = def
            .get(STORYBOARD_SECTION, StoryboardKind::Ending.def_key())
            .map(str::trim)
            .is_some_and(|s| !s.is_empty());
        assert!(!has_intro, "no intro.storyboard declared -> no overlay");
        assert!(!has_ending, "no ending.storyboard declared -> no overlay");
    }

    /// The real KFM character `.def` declares both an intro and an ending
    /// storyboard, and both resolve to existing `.def` files (asset-gated; skips
    /// cleanly when test-assets is absent). This verifies the declaration-reading
    /// half of the overlay loader against real content without needing a GPU.
    #[test]
    fn kfm_declares_resolvable_storyboards() {
        let char_def = test_asset("kfm/kfm.def");
        let Ok(def) = fp_formats::def::DefFile::load(&char_def) else {
            eprintln!("skipping: {} not present", char_def.display());
            return;
        };
        for kind in [StoryboardKind::Intro, StoryboardKind::Ending] {
            let rel = def
                .get(STORYBOARD_SECTION, kind.def_key())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| panic!("KFM declares {}", kind.def_key()));
            let sb_path = fp_formats::def::DefFile::resolve_path(&char_def, rel);
            assert!(
                sb_path.exists(),
                "KFM {} resolves to an existing file: {}",
                kind.def_key(),
                sb_path.display()
            );
            // And the storyboard parses into a non-empty, playable scene model.
            let sb = fp_storyboard::Storyboard::load(&sb_path).expect("storyboard loads");
            assert!(
                !sb.scenes.is_empty(),
                "KFM {} has scenes",
                kind.def_key()
            );
            assert!(
                !sb.sprite_path.is_empty(),
                "KFM {} declares a sprite file",
                kind.def_key()
            );
        }
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

    // =====================================================================
    // PR-L — stage background wiring (audit #29)
    // =====================================================================

    /// A fighter's screen anchor scrolls opposite the camera: with the camera at
    /// 0 it matches the no-stage mapping; moving the camera right (+) shifts the
    /// fighter left on screen by the same amount (`pos.x - camera_x`).
    #[test]
    fn player_anchor_scrolls_with_camera() {
        let win_w = 640.0;
        let win_h = 480.0;
        let pos = fp_core::Vec2::new(60.0, 0.0);

        let (no_cam, _) = player_screen_anchor(pos, 0.0, win_w, win_h);
        assert!(
            (no_cam - world_to_screen_x(60.0, win_w)).abs() < 1e-6,
            "camera 0 reduces to the original centered mapping (no regression)"
        );

        let (with_cam, _) = player_screen_anchor(pos, 100.0, win_w, win_h);
        assert!(
            (with_cam - world_to_screen_x(-40.0, win_w)).abs() < 1e-6,
            "camera +100 maps pos.x-100 (the world scrolls under the camera)"
        );
        assert!(with_cam < no_cam, "panning the camera right pushes the fighter left");
    }

    /// `bg_sprite_id` accepts in-range group/image and rejects (with `None`) any
    /// component outside the SFF `u16` range rather than wrapping to a wrong id.
    #[test]
    fn bg_sprite_id_validates_u16_range() {
        assert_eq!(bg_sprite_id(0, 0), Some(SpriteId::new(0, 0)));
        assert_eq!(bg_sprite_id(65535, 65535), Some(SpriteId::new(65535, 65535)));
        assert_eq!(bg_sprite_id(-1, 0), None, "negative group is out of range");
        assert_eq!(bg_sprite_id(0, 70000), None, "image > u16::MAX is out of range");
    }

    /// `stage_arg` picks up a `.def` extra argument as the stage and ignores a
    /// non-`.def` one (keeping the flat background).
    #[test]
    fn stage_arg_selects_def_extra_only() {
        let args = vec![
            "fp-app".to_string(),
            "p1.def".to_string(),
            "p2.def".to_string(),
            "ringside.def".to_string(),
        ];
        assert_eq!(stage_arg(&args, 3), Some(Path::new("ringside.def")));
        // Out-of-range index → no stage.
        assert_eq!(stage_arg(&args, 9), None);

        let non_def = vec![
            "fp-app".to_string(),
            "p1.def".to_string(),
            "p2.def".to_string(),
            "notes.txt".to_string(),
        ];
        assert_eq!(stage_arg(&non_def, 3), None, "a non-.def extra arg is not a stage");
    }

    /// A missing stage path degrades to no stage (the flat-background fallback)
    /// rather than panicking — `StageRender::load` returns `None`.
    #[test]
    fn stage_load_missing_file_is_none_not_panic() {
        let missing = Path::new("/no/such/stage/definitely-not-here.def");
        assert!(StageRender::load(missing).is_none());
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
        match build_two_player_match(&def, &def, PalSelection::default()) {
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
        let player = match build_player(&def, P1_START_X, None) {
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
        let mut solo = match build_player(&test_asset("kfm/kfm.def"), P1_START_X, None) {
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
        let (p1, p2) = match (build_player(&def, P1_START_X, None), build_player(&def, P2_START_X, None)) {
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
        let (p1, p2) = match (build_player(&def, P1_START_X, None), build_player(&def, P2_START_X, None)) {
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

    // --- Clsn debug overlay box-mapping math (audit #34) ---

    #[test]
    fn player_screen_anchor_matches_draw_player_mapping() {
        // The overlay anchor must equal the (x, y) draw_player hangs the sprite
        // off: X via world_to_screen_x, Y via the ground plane plus pos.y.
        let win_w = 640.0;
        let win_h = 480.0;
        let pos = fp_core::Vec2::new(40.0, -30.0);
        // Camera at 0: the anchor reduces to the original (un-scrolled) mapping.
        let (ax, ay) = player_screen_anchor(pos, 0.0, win_w, win_h);
        assert!((ax - world_to_screen_x(pos.x, win_w)).abs() < 1e-4);
        assert!((ay - (win_h * 0.8 + pos.y)).abs() < 1e-4);
    }

    #[test]
    fn clsn_to_screen_box_facing_right_translates_only() {
        // A local box to the right of and above the axis, facing right: X/Y are
        // just translated by the anchor; no mirroring.
        let local = fp_core::Rect::new(10.0, -40.0, 20.0, 30.0); // x,y,w,h
        let b = clsn_to_screen_box(&local, 100.0, 200.0, fp_character::Facing::Right, CLSN1_COLOR);
        assert!((b.x - 110.0).abs() < 1e-4, "x = anchor_x + local.x");
        assert!((b.w - 20.0).abs() < 1e-4, "width preserved");
        assert!((b.y - 160.0).abs() < 1e-4, "y = anchor_y + local.y (Y down)");
        assert!((b.h - 30.0).abs() < 1e-4, "height preserved");
        assert_eq!(b.color, CLSN1_COLOR);
    }

    #[test]
    fn clsn_to_screen_box_facing_left_mirrors_x_only() {
        // Same local box facing left: X is reflected about the anchor while Y is
        // untouched, matching fp_physics::place_clsn. The left/right edges swap,
        // but the result stays a non-negative-width rect.
        let local = fp_core::Rect::new(10.0, -40.0, 20.0, 30.0);
        let right =
            clsn_to_screen_box(&local, 100.0, 200.0, fp_character::Facing::Right, CLSN2_COLOR);
        let left =
            clsn_to_screen_box(&local, 100.0, 200.0, fp_character::Facing::Left, CLSN2_COLOR);

        // Facing left: edges run from anchor - local.right() to anchor - local.x.
        assert!((left.x - 70.0).abs() < 1e-4, "left edge = anchor_x - local.right()");
        assert!((left.w - 20.0).abs() < 1e-4, "width preserved under mirroring");
        // Y is identical to the right-facing case (facing never affects Y).
        assert!((left.y - right.y).abs() < 1e-4);
        assert!((left.h - right.h).abs() < 1e-4);
        // The mirrored box is the reflection of the right-facing box about the
        // anchor X: their centers are equidistant from the anchor.
        let rc = right.x + right.w / 2.0;
        let lc = left.x + left.w / 2.0;
        assert!(((100.0 - lc) - (rc - 100.0)).abs() < 1e-4, "symmetric about anchor");
    }

    #[test]
    fn clsn_to_screen_box_axis_crossing_box_facing_left() {
        // A box straddling the axis (negative left edge) still normalizes to a
        // non-negative width after the left-facing mirror.
        let local = fp_core::Rect::new(-15.0, -10.0, 30.0, 10.0); // spans x=-15..15
        let b = clsn_to_screen_box(&local, 50.0, 0.0, fp_character::Facing::Left, CLSN1_COLOR);
        assert!(b.w >= 0.0, "width is non-negative after mirroring");
        assert!((b.w - 30.0).abs() < 1e-4, "width magnitude preserved");
        // Mirrored edges: anchor - 15 .. anchor + 15 => 35..65, so x = 35.
        assert!((b.x - 35.0).abs() < 1e-4);
    }

    // ---- #16: two-fighter draw-order decision from `sprpriority` ----

    #[test]
    fn p1_draws_behind_p2_orders_by_sprpriority() {
        // Lower priority draws first (behind). P1 lower -> P1 behind (true).
        assert!(p1_draws_behind_p2(0, 2), "lower P1 priority draws behind P2");
        // P1 higher -> P1 in front -> P2 drawn first (false).
        assert!(!p1_draws_behind_p2(5, 1), "higher P1 priority draws in front of P2");
        // Negative priorities order the same way (lower behind).
        assert!(p1_draws_behind_p2(-3, 0), "more-negative P1 draws behind");
        assert!(!p1_draws_behind_p2(1, -1), "P1 above a negative P2 draws in front");
    }

    #[test]
    fn p1_draws_behind_p2_tie_keeps_p1_behind() {
        // Equal priorities: stable, deterministic default — P1 behind P2.
        assert!(p1_draws_behind_p2(2, 2), "a tie keeps P1 behind P2 (stable order)");
        assert!(p1_draws_behind_p2(0, 0), "default-priority tie keeps P1 behind");
    }

    #[test]
    fn char_palfx_to_render_inactive_maps_to_identity() {
        // The accessors hand back CurPalFx::IDENTITY when nothing is active; that
        // must convert to the renderer's no-op identity (no is_active gate needed).
        let out = char_palfx_to_render(fp_character::CurPalFx::IDENTITY);
        assert_eq!(out, fp_render::PalFx::IDENTITY);
        assert!(out.is_identity(), "inactive effect renders byte-identically");
    }

    #[test]
    fn char_palfx_to_render_passes_active_fields_through() {
        // An active tint's add/mul/color cross the crate boundary unchanged.
        let fx = fp_character::CurPalFx {
            add: [0.5, -0.25, 0.0],
            mul: [0.5, 1.0, 2.0],
            color: 0.25,
            remaining: 12,
        };
        let out = char_palfx_to_render(fx);
        assert_eq!(out.add, [0.5, -0.25, 0.0]);
        assert_eq!(out.mul, [0.5, 1.0, 2.0]);
        assert!((out.color - 0.25).abs() < 1e-6);
        assert!(!out.is_identity(), "an active tint is not the no-op");
    }

    // =====================================================================
    // PR-N — fight.def screenpack HUD wiring (audit #31)
    // =====================================================================

    /// With no `fight.def` anywhere on the search path (and no `FP_SCREENPACK`),
    /// `locate_fight_def` finds nothing — so the match falls back to the quad HUD
    /// (no regression for the default, screenpack-less match).
    #[test]
    fn locate_fight_def_returns_none_when_absent() {
        // A character .def in a directory that contains no fight.def / data/fight.def.
        let dir = std::env::temp_dir().join(format!("fp-screenpack-none-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p1_def = dir.join("kfm.def");
        // Guard against a leaked FP_SCREENPACK from the environment.
        std::env::remove_var("FP_SCREENPACK");
        assert!(
            locate_fight_def(&p1_def).is_none(),
            "no fight.def -> None -> quad HUD fallback"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `locate_fight_def` finds a `fight.def` sitting next to the P1 character def.
    #[test]
    fn locate_fight_def_finds_sibling() {
        let dir = std::env::temp_dir().join(format!("fp-screenpack-sib-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fight = dir.join("fight.def");
        std::fs::write(&fight, "[Files]\nsff = fight.sff\n").unwrap();
        let p1_def = dir.join("kfm.def");
        std::env::remove_var("FP_SCREENPACK");
        assert_eq!(
            locate_fight_def(&p1_def).as_deref(),
            Some(fight.as_path()),
            "a sibling fight.def is located"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The round announcer text reflects the decided round (KO / win / draw), the
    /// round number during the intro, and is empty mid-fight.
    ///
    /// Exercises the **production** [`round_label`] (the very mapping
    /// `round_readout` delegates to), not a test-only copy — so a divergence in
    /// the real code cannot pass silently.
    #[test]
    fn round_readout_reflects_state() {
        // KO marker wins regardless of winner.
        assert_eq!(round_label(RoundState::Ko, None, 1), "KO");
        assert_eq!(round_label(RoundState::Win, Some(Winner::P1), 1), "P1 WINS");
        assert_eq!(round_label(RoundState::Win, Some(Winner::P2), 1), "P2 WINS");
        assert_eq!(round_label(RoundState::Win, None, 1), "DRAW");
        // The intro readout reflects the live round number (not a fixed "1").
        assert_eq!(round_label(RoundState::Intro, None, 1), "ROUND 1");
        assert_eq!(round_label(RoundState::Intro, None, 3), "ROUND 3");
        assert_eq!(round_label(RoundState::Fight, None, 1), "");
    }

    /// The HUD timer is whole seconds, not the engine's frames-remaining clock:
    /// `Match::timer()` returns frames (60/s), so a 99-second round (5940 frames)
    /// must read 99, never 5940. Guards the screenpack-path timer-unit contract.
    #[test]
    fn timer_frames_convert_to_whole_seconds() {
        assert_eq!(timer_frames_to_seconds(5940), 99, "99s round = 5940 frames -> 99");
        assert_eq!(timer_frames_to_seconds(60), 1);
        assert_eq!(timer_frames_to_seconds(0), 0);
        // Floors mid-second (sub-60 remainder is dropped, MUGEN-style).
        assert_eq!(timer_frames_to_seconds(59), 0);
        assert_eq!(timer_frames_to_seconds(119), 1);
        // Negative (shouldn't happen, but never render "-1").
        assert_eq!(timer_frames_to_seconds(-30), 0);
    }

    // ---- HUD text + .act palette selection (FL2b) ----------------------------

    /// Resolves a path under the shipped (committed) `assets/data/` directory.
    /// `CARGO_MANIFEST_DIR` points at `crates/fp-app`; go up two levels.
    fn shipped_data_asset(rel: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../assets/data")
            .join(rel)
    }

    /// The HUD timer text is the engine's frame clock converted to whole seconds
    /// (the pure composition the announcer draws), floored and never negative.
    #[test]
    fn timer_text_is_whole_seconds_string() {
        assert_eq!(timer_text(5940), "99"); // 99s round
        assert_eq!(timer_text(60), "1");
        assert_eq!(timer_text(0), "0");
        assert_eq!(timer_text(59), "0"); // floors the sub-second remainder
        assert_eq!(timer_text(-30), "0"); // never renders a negative
    }

    /// The shipped HUD font (FL2b) parses through the real loader and maps every
    /// character the announcer/timer strings need. This is the non-gated, shipped
    /// asset, so it must load on every machine. (GPU upload via `GlyphFont` is not
    /// exercised here — that needs a device; the parse + glyph map is the contract
    /// the HUD text path depends on.)
    #[test]
    fn shipped_hud_font_parses_and_covers_hud_strings() {
        let font = fp_formats::fnt::FntFont::load(&shipped_data_asset("font.fnt"))
            .expect("shipped HUD font.fnt must load");
        // Every character used by ROUND N / KO / P1 WINS / P2 WINS / DRAW and the
        // digit timer must be mapped.
        for s in ["KO", "ROUND", "WINS", "DRAW", "P1", "P2", "0123456789", " ", ":"] {
            for c in s.chars() {
                assert!(
                    font.glyph(c).is_some(),
                    "HUD font is missing glyph {c:?} (needed for HUD text)"
                );
            }
        }
        assert!(font.image_height > 0, "font must have a real line height");
    }

    /// `parse_pal_flags` extracts `--p1-pal`/`--p2-pal` and leaves the positional
    /// args (program name + paths) untouched for the rest of CLI routing.
    #[test]
    fn parse_pal_flags_extracts_selections_and_keeps_paths() {
        let args: Vec<String> = ["fp-app", "kfm.def", "--p1-pal", "3", "ken.def", "--p2-pal", "7"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (sel, rest) = parse_pal_flags(&args);
        assert_eq!(sel, PalSelection { p1: Some(3), p2: Some(7) });
        // The flags (and their values) are stripped; the positional args survive
        // in order so `select_mode`'s file routing is unchanged.
        assert_eq!(rest, vec!["fp-app", "kfm.def", "ken.def"]);
    }

    /// No flags → no selection and the args pass through byte-for-byte, so the
    /// default (no costume swap) path is unchanged.
    #[test]
    fn parse_pal_flags_default_is_none_and_passthrough() {
        let args: Vec<String> = ["fp-app", "a.def", "b.def"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (sel, rest) = parse_pal_flags(&args);
        assert_eq!(sel, PalSelection::default());
        assert_eq!(sel.p1, None);
        assert_eq!(sel.p2, None);
        assert_eq!(rest, args);
    }

    /// A `--pN-pal` with a missing or non-numeric value is dropped (the selection
    /// stays `None`) and never panics; unknown `--…` tokens pass through.
    #[test]
    fn parse_pal_flags_tolerates_bad_values() {
        // Missing value at end-of-args.
        let a: Vec<String> = ["fp-app", "--p1-pal"].iter().map(|s| s.to_string()).collect();
        let (sel, rest) = parse_pal_flags(&a);
        assert_eq!(sel.p1, None);
        assert_eq!(rest, vec!["fp-app"]);

        // Non-numeric value: the value token is NOT consumed as a selection; it is
        // left as a positional arg.
        let b: Vec<String> = ["fp-app", "--p2-pal", "x.def"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (sel, rest) = parse_pal_flags(&b);
        assert_eq!(sel.p2, None);
        assert_eq!(rest, vec!["fp-app", "x.def"]);

        // An unrelated `--flag` is passed through untouched.
        let c: Vec<String> = ["fp-app", "--verbose", "a.def"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (_sel, rest) = parse_pal_flags(&c);
        assert_eq!(rest, vec!["fp-app", "--verbose", "a.def"]);
    }

    /// The `.act` override binding seam (the decision `get_or_create_sprite` /
    /// `PaletteTexture::from_override` make): a present override binds a palette
    /// that differs from the embedded one (a visible costume swap), while an absent
    /// override binds the embedded palette byte-for-byte (the default,
    /// no-regression path). Pins the precedence the GPU upload depends on without
    /// needing a device. `bind_palette` mirrors `from_override`'s pure chooser.
    #[test]
    fn act_override_binds_when_selected_else_embedded() {
        /// The exact rule `PaletteTexture::from_override` applies: the override
        /// bytes when present, otherwise the embedded palette.
        fn bind_palette<'a>(
            embedded: &'a [u8; 1024],
            override_rgba: Option<&'a [u8; 1024]>,
        ) -> &'a [u8; 1024] {
            override_rgba.unwrap_or(embedded)
        }

        let mut embedded = [0u8; 1024];
        // Give the embedded palette a recognizable color at index 1.
        embedded[4] = 10;
        embedded[5] = 20;
        embedded[6] = 30;
        let mut over = embedded;
        // The override differs at index 1 (a costume swap).
        over[4] = 200;
        over[5] = 100;
        over[6] = 50;

        // With a selection: the bound bytes are the override and differ from
        // embedded (the costume is visibly different).
        let chosen_with = bind_palette(&embedded, Some(&over));
        assert!(std::ptr::eq(chosen_with, &over));
        assert_ne!(
            &chosen_with[4..7],
            &embedded[4..7],
            "a selected .act override must bind a palette that differs from embedded"
        );
        assert_eq!(&chosen_with[4..7], &[200, 100, 50]);

        // With no selection: the bound bytes are the embedded palette,
        // byte-identical (the default, no-regression path).
        let chosen_without = bind_palette(&embedded, None);
        assert!(std::ptr::eq(chosen_without, &embedded));
        assert_eq!(chosen_without, &embedded);
    }

    /// `build_player` applies a valid `.act` selection to the entity's active
    /// palette and ignores an out-of-range one (falling back to embedded), never
    /// panicking. Gated on the local KFM asset; KFM ships no `.act` overrides, so
    /// any selection is out-of-range and must resolve to "no active palette".
    #[test]
    fn build_player_palette_selection_is_safe() {
        let def = test_asset("kfm/kfm.def");
        if !def.exists() {
            eprintln!("skipping palette-selection test: {} not present", def.display());
            return;
        }
        // KFM has no `.act` overrides, so selecting one is out of range: the
        // entity must keep the embedded palette (active_palette == None), with a
        // warning, not a panic.
        let player = match build_player(&def, P1_START_X, Some(2)) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("skipping palette-selection test: {e}");
                return;
            }
        };
        assert_eq!(
            player.character.active_palette(),
            None,
            "an out-of-range .act selection must fall back to the embedded palette"
        );
        // And no selection leaves the embedded palette too.
        let plain = build_player(&def, P1_START_X, None).expect("kfm loads");
        assert_eq!(plain.character.active_palette(), None);
    }

    /// With NO explicit `--pN-pal` flag, a character that ships `.act` palettes
    /// (evilken) defaults to its costume palette (pal1, index 0) rather than the
    /// SFF-embedded one — the black-screen fix. An explicit flag still overrides
    /// the default. Gated on the local evilken asset.
    #[test]
    fn build_player_defaults_to_costume_palette_when_act_present() {
        let def = test_asset("evilken/evilken.def");
        if !def.exists() {
            eprintln!("skipping: {} not present", def.display());
            return;
        }
        // No flag → defaults to pal1 (the costume), not the SFF-embedded palette.
        let Ok(defaulted) = build_player(&def, P1_START_X, None) else {
            eprintln!("skipping: evilken.def failed to build");
            return;
        };
        assert_eq!(
            defaulted.character.active_palette(),
            Some(0),
            "evilken must default to its costume palette (pal1) so it renders in color"
        );
        // An explicit flag still wins over the default.
        let chosen = build_player(&def, P1_START_X, Some(3)).expect("evilken builds");
        assert_eq!(
            chosen.character.active_palette(),
            Some(3),
            "an explicit --pN-pal flag must override the default costume"
        );
    }

    // ---- common-effects (fightfx) load path (audit #17) ----------------------

    /// Resolves a path under the shipped (committed) `assets/data/` directory.
    fn shipped_fx_asset(rel: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../assets/data")
            .join(rel)
    }

    /// The shipped common-effects asset loads end to end: the SFF parses with
    /// spark sprites and the AIR authors KFM's common spark actions. This is the
    /// path `load_common_fx` takes at startup (here with absolute paths so it runs
    /// regardless of the test CWD).
    #[test]
    fn shipped_common_fx_loads() {
        let sff = shipped_fx_asset("fightfx.sff");
        let air = shipped_fx_asset("fightfx.air");
        let loaded = load_common_fx_from(&sff, &air)
            .expect("shipped common-fx asset must load");
        let (air, render) = loaded;
        // The render bundle carries the parsed SFF with sprites.
        assert!(
            !render.sff.sprites.is_empty(),
            "fightfx render bundle must carry spark sprites"
        );
        // The AIR authors the standard KFM common spark actions.
        for g in [0, 1, 2, 3, 40] {
            assert!(air.action(g).is_some(), "fightfx.air must author common action {g}");
        }
    }

    /// A missing common-fx asset is a clean best-effort `None` — never a panic.
    #[test]
    fn missing_common_fx_is_none_not_panic() {
        let missing_sff = shipped_fx_asset("does-not-exist.sff");
        let missing_air = shipped_fx_asset("does-not-exist.air");
        assert!(
            load_common_fx_from(&missing_sff, &missing_air).is_none(),
            "an absent common-fx asset must yield None, not a panic"
        );
        // A present SFF but absent AIR (and vice versa) is also a clean None.
        let real_sff = shipped_fx_asset("fightfx.sff");
        assert!(
            load_common_fx_from(&real_sff, &missing_air).is_none(),
            "a half-present common-fx asset must yield None"
        );
    }

    /// Gated end-to-end (skip-if-KFM-missing): with the shipped common-fx AIR
    /// installed on a real two-KFM match, a connecting KFM hit spawns a common
    /// ([`EffectSide::Common`]) spark — the user-visible FL2a outcome wired
    /// through the app's own match build path.
    #[test]
    fn kfm_match_with_common_fx_spawns_common_spark() {
        let def = test_asset("kfm/kfm.def");
        if !def.exists() {
            eprintln!("skipping: {} not present", def.display());
            return;
        }
        // The fightfx AIR ships, so it must load (not gated on it).
        let air = AirFile::load(&shipped_fx_asset("fightfx.air"))
            .expect("shipped fightfx.air must load");

        let mut m = match build_two_player_match(&def, &def, PalSelection::default()) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("skipping: KFM failed to build: {e}");
                return;
            }
        };
        m.set_common_fx(air);

        // Run intro out, walk into range, and punch until a hit lands.
        for _ in 0..400 {
            m.tick(MatchInput::none(), MatchInput::none());
            if m.round_state() == RoundState::Fight {
                break;
            }
        }
        for _ in 0..240 {
            m.tick(MatchInput { right: true, ..MatchInput::none() }, MatchInput::none());
            if (m.p1().pos().x - m.p2().pos().x).abs() <= 40.0 {
                break;
            }
        }
        let p2_before = m.p2().life();
        let mut saw_common = false;
        for i in 0..400 {
            let inp = if i % 3 == 0 {
                MatchInput { x: true, ..MatchInput::none() }
            } else {
                MatchInput::none()
            };
            m.tick(inp, MatchInput::none());
            if m.effects().iter().any(|fx| fx.side == EffectSide::Common) {
                saw_common = true;
            }
            if m.p2().life() < p2_before {
                break;
            }
            if m.round_state() != RoundState::Fight {
                break;
            }
        }
        assert!(
            saw_common,
            "a real KFM hit (common sparkno) must spawn a common fightfx spark with the asset loaded"
        );
    }

    // =====================================================================
    // Menu-2 — in-app screen state machine (Title -> Select -> Fight -> Title)
    // =====================================================================

    /// Resolves a path under the workspace `assets/` directory (the shipped,
    /// clean-room default motif). `CARGO_MANIFEST_DIR` is `crates/fp-app`.
    fn shipped_asset(rel: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../assets")
            .join(rel)
    }

    /// No file argument routes to the Title menu; an explicit `menu` token does
    /// too. Any direct content path keeps the legacy direct route (no menu), so
    /// the existing CLI is preserved.
    #[test]
    fn cli_route_menu_vs_direct() {
        // No args -> Menu.
        assert_eq!(cli_route(&["fp-app".to_string()]), CliRoute::Menu);
        // Explicit `menu` (case-insensitive) -> Menu.
        assert_eq!(
            cli_route(&["fp-app".to_string(), "menu".to_string()]),
            CliRoute::Menu
        );
        assert_eq!(
            cli_route(&["fp-app".to_string(), "MENU".to_string()]),
            CliRoute::Menu
        );
        // A character .def -> Direct (the legacy two-player path).
        assert_eq!(
            cli_route(&["fp-app".to_string(), "kfm.def".to_string()]),
            CliRoute::Direct
        );
        // Two .defs -> Direct.
        assert_eq!(
            cli_route(&[
                "fp-app".to_string(),
                "p1.def".to_string(),
                "p2.def".to_string()
            ]),
            CliRoute::Direct
        );
        // An .sff (viewer) -> Direct.
        assert_eq!(
            cli_route(&["fp-app".to_string(), "kfm.sff".to_string()]),
            CliRoute::Direct
        );
    }

    /// The default motif loads its declared roster (the shipped `select.def`,
    /// which lists Training Dummy twice + a randomselect icon), so the title menu
    /// and select grid build from real shipped content. Asset-gated on the shipped
    /// motif (always present in this worktree); skips cleanly if absent.
    #[test]
    fn default_motif_loads_shipped_roster() {
        let system_path = shipped_asset("data/system.def");
        let fallback = shipped_asset("data/select.def");
        if !system_path.exists() {
            eprintln!("skipping: {} not present", system_path.display());
            return;
        }
        let motif = Motif::load_from(&system_path, &fallback);
        // The shipped system.def enables VS MODE / TRAINING / EXIT.
        let title = screens::TitleMenu::from_system(&motif.system);
        let labels: Vec<&str> = title.entries.iter().map(|e| e.label.as_str()).collect();
        assert!(labels.contains(&"VS MODE"), "title has VS MODE: {labels:?}");
        assert!(labels.contains(&"TRAINING"), "title has TRAINING");
        assert!(labels.contains(&"EXIT"), "title has EXIT");
        // The roster has at least one choosable character (Training Dummy).
        let select = screens::SelectScreen::new(
            screens::SelectMode::Training,
            &motif.select,
            &motif.system.select_info,
            &motif.select_path,
        );
        assert!(!select.is_empty(), "shipped roster has a choosable character");
    }

    /// A missing motif resolves to the built-in fallback title menu (VS /
    /// TRAINING / EXIT) over an empty roster, without panicking — the clean-room
    /// degradation path.
    #[test]
    fn missing_motif_uses_fallback_menu() {
        let motif = Motif::load_from(
            Path::new("/no/such/system.def"),
            Path::new("/no/such/select.def"),
        );
        let title = screens::TitleMenu::from_system(&motif.system);
        // Built-in fallback ships exactly VS / TRAINING / EXIT.
        assert_eq!(title.entries.len(), 3);
        assert_eq!(title.entries[0].label, "VS MODE");
        // Empty roster: the select screen reports empty (the app would stay on
        // title), never a panic.
        let select = screens::SelectScreen::new(
            screens::SelectMode::Versus,
            &motif.select,
            &motif.system.select_info,
            &motif.select_path,
        );
        assert!(select.is_empty(), "no roster -> empty select screen");
    }

    /// End-to-end headless flow: select the shipped trainingdummy roster (the
    /// Training-mode single confirm), resolve the pick to its `.def`, load it, and
    /// build + drive a real two-player [`Match`] for a few ticks. Verifies the
    /// menu's roster-pick -> load -> Match wiring actually produces a runnable
    /// match over the shipped clean-room character (no KFM needed). Asset-gated on
    /// the shipped motif/character.
    #[test]
    fn training_pick_builds_and_drives_a_match() {
        let system_path = shipped_asset("data/system.def");
        let fallback = shipped_asset("data/select.def");
        if !system_path.exists() {
            eprintln!("skipping: {} not present", system_path.display());
            return;
        }
        let motif = Motif::load_from(&system_path, &fallback);
        let mut select = screens::SelectScreen::new(
            screens::SelectMode::Training,
            &motif.select,
            &motif.system.select_info,
            &motif.select_path,
        );
        // Single confirm in Training picks P1 (cell 0) and mirrors P2.
        let confirm = screens::MenuInput {
            confirm: true,
            ..screens::MenuInput::default()
        };
        let screens::SelectOutcome::Done(pick) = select.update(confirm, 0) else {
            panic!("training confirm should complete the select screen");
        };
        // The pick resolves to a real, existing trainingdummy .def under assets/.
        assert_eq!(
            pick.p1_def, pick.p2_def,
            "training mirrors P1 onto P2 (idle dummy)"
        );
        assert!(
            pick.p1_def.exists(),
            "picked .def resolves to an existing file: {}",
            pick.p1_def.display()
        );
        // Build the same two-player Match the menu Fight screen builds, and drive
        // a handful of ticks. It must not panic and must start in the intro phase
        // with both fighters at full life.
        let mut m = build_two_player_match(&pick.p1_def, &pick.p2_def, PalSelection::default())
            .expect("trainingdummy match builds");
        assert_eq!(m.round_state(), RoundState::Intro);
        assert_eq!(m.p1().life(), m.p1().life_max());
        assert!(m.match_winner().is_none(), "no winner at the start");
        for _ in 0..30 {
            m.tick(MatchInput::none(), MatchInput::none());
        }
        // After a few ticks the match is still coherent (no panic; both alive).
        assert!(m.p1().life() >= 0);
        assert!(m.p2().life() >= 0);
    }

    /// `resolve_select_path` prefers the motif's declared `[Files] select`
    /// (resolved relative to the system.def) when it exists, and falls back to the
    /// shipped path otherwise. Uses the shipped motif where `select = select.def`
    /// sits next to `system.def`.
    #[test]
    fn resolve_select_path_prefers_declared_then_fallback() {
        let system_path = shipped_asset("data/system.def");
        if !system_path.exists() {
            eprintln!("skipping: {} not present", system_path.display());
            return;
        }
        let def = fp_formats::def::DefFile::load(&system_path).expect("load system.def");
        let system = SystemDef::parse(&def);
        let fallback = Path::new("/no/such/fallback-select.def");
        let resolved = resolve_select_path(&system_path, &system, fallback);
        // The shipped motif's declared select.def sits next to system.def.
        assert!(resolved.exists(), "declared select.def resolved: {}", resolved.display());
        assert!(
            resolved.ends_with("select.def"),
            "resolved to the declared select.def"
        );

        // A motif declaring a missing select falls back to the given path.
        let bad = SystemDef {
            select_file: "definitely-missing.def".to_string(),
            ..SystemDef::default()
        };
        let fb = resolve_select_path(&system_path, &bad, fallback);
        assert_eq!(fb, fallback, "missing declared select -> fallback path");
    }
}
