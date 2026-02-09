//! Fighters Paradise — a modern MUGEN engine reimplementation in Rust.
//!
//! This is the application entry point. It initializes the SDL2 window,
//! sets up the wgpu rendering pipeline, and runs the main 60Hz game loop.
//!
//! # Usage
//!
//! ```text
//! cargo run -p fp-app -- <file.sff> <file.air> [file.cmd]  # playable character
//! cargo run -p fp-app -- <file.sff> <file.air>              # animation viewer
//! cargo run -p fp-app -- <file.sff>                         # show first sprite
//! cargo run -p fp-app                                       # checkerboard test pattern
//! ```
//!
//! When an SFF, AIR, and optional CMD file are provided, the character becomes
//! playable with walking, jumping, and crouching. Arrow keys (or WASD) move
//! the character, and attack buttons are mapped to U/I/O/J/K/L.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use fp_core::SpriteId;
use fp_formats::air::{AirFile, AnimAction};
use fp_formats::cmd::CmdFile;
use fp_formats::sff::SffFile;
use fp_input::{compile_command, CommandDef, CommandMatcher, InputBuffer, InputState};
use fp_physics::PhysicsBody;
use fp_render::{AnimController, PaletteTexture, Renderer, SpriteDrawParams, SpriteTexture};
use sdl2::event::Event;
use sdl2::keyboard::{Keycode, Scancode};

/// Window width in pixels.
const WINDOW_WIDTH: u32 = 640;
/// Window height in pixels.
const WINDOW_HEIGHT: u32 = 480;
/// Fixed timestep duration: 1/60th of a second (~16.667ms).
const TICK_DURATION: Duration = Duration::from_nanos(16_666_667);

// --- Hardcoded KFM physics constants (until CNS parser in Phase 5) ---

/// Walk forward velocity (pixels per tick).
const WALK_FWD_VEL: f32 = 2.4;
/// Walk backward velocity (pixels per tick).
const WALK_BACK_VEL: f32 = 2.2;
/// Jump initial vertical velocity.
const JUMP_VEL_Y: f32 = -8.4;
/// Jump forward horizontal velocity.
const JUMP_FWD_VEL_X: f32 = 2.5;
/// Gravity acceleration (positive = pulls toward ground).
const GRAVITY: f32 = 0.44;
/// Duration of jump start animation (ticks).
const JUMP_START_TICKS: u32 = 4;
/// Duration of landing animation (ticks).
const LANDING_TICKS: u32 = 3;

// --- Common state numbers (MUGEN standard) ---

/// Idle/standing state.
const STATE_IDLE: i32 = 0;
/// Walk forward state.
const STATE_WALK_FWD: i32 = 12;
/// Walk backward state.
const STATE_WALK_BACK: i32 = 13;
/// Crouching state.
const STATE_CROUCH: i32 = 20;
/// Jump start (pre-jump) state.
const STATE_JUMP_START: i32 = 40;
/// Airborne state.
const STATE_AIRBORNE: i32 = 50;
/// Landing recovery state.
const STATE_LANDING: i32 = 52;

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
// Playable character (Phase 3)
// ---------------------------------------------------------------------------

/// A playable character with physics, input, and a simple state machine.
struct PlayableCharacter {
    sff: SffFile,
    air: AirFile,
    anim: AnimController,
    physics: PhysicsBody,
    input_buffer: InputBuffer,
    command_matcher: Option<CommandMatcher>,
    sprite_cache: HashMap<SpriteId, CachedSprite>,
    /// Current state number.
    state: i32,
    /// Ticks spent in the current state.
    state_time: u32,
    /// `true` = facing right, `false` = facing left.
    facing_right: bool,
}

impl PlayableCharacter {
    fn new(sff: SffFile, air: AirFile, cmd: Option<CmdFile>) -> Self {
        let idle_action = air
            .action(0)
            .cloned()
            .unwrap_or(AnimAction {
                action_number: 0,
                frames: vec![],
                loopstart: 0,
            });

        let command_matcher = cmd.map(|cmd_file| {
            let defs: Vec<CommandDef> = cmd_file
                .commands
                .iter()
                .filter_map(|c| {
                    let elements = compile_command(&c.command).ok()?;
                    Some(CommandDef {
                        name: c.name.clone(),
                        elements,
                        time: c.time,
                        buffer_time: c.buffer_time,
                    })
                })
                .collect();
            tracing::info!("Compiled {} commands from CMD file", defs.len());
            CommandMatcher::new(defs)
        });

        tracing::info!(
            "Playable character: {} actions loaded",
            air.actions.len()
        );

        Self {
            sff,
            air,
            anim: AnimController::new(idle_action),
            physics: PhysicsBody::new(0.0, 0.0),
            input_buffer: InputBuffer::new(),
            command_matcher,
            sprite_cache: HashMap::new(),
            state: STATE_IDLE,
            state_time: 0,
            facing_right: true,
        }
    }

    /// Change to a new state, resetting state_time and switching animation.
    fn change_state(&mut self, new_state: i32) {
        if new_state == self.state {
            return;
        }
        self.state = new_state;
        self.state_time = 0;

        let action_num = state_to_action(new_state);
        if let Some(action) = self.air.action(action_num) {
            self.anim.set_action(action.clone());
        } else {
            tracing::warn!("No animation for action {action_num} (state {new_state})");
        }
    }

    /// Run one tick of the state machine.
    fn tick(&mut self) {
        let input = self
            .input_buffer
            .get(0)
            .copied()
            .unwrap_or_default();

        let up = input.direction.up;
        let down = input.direction.down;
        let forward = if self.facing_right {
            input.direction.right
        } else {
            input.direction.left
        };
        let back = if self.facing_right {
            input.direction.left
        } else {
            input.direction.right
        };

        let facing_sign = if self.facing_right { 1.0 } else { -1.0 };

        match self.state {
            STATE_IDLE => {
                self.physics.vel.x = 0.0;
                if up {
                    self.change_state(STATE_JUMP_START);
                } else if forward {
                    self.change_state(STATE_WALK_FWD);
                } else if back {
                    self.change_state(STATE_WALK_BACK);
                } else if down {
                    self.change_state(STATE_CROUCH);
                }
            }
            STATE_WALK_FWD => {
                self.physics.vel.x = WALK_FWD_VEL * facing_sign;
                if up {
                    self.change_state(STATE_JUMP_START);
                } else if !forward {
                    self.change_state(STATE_IDLE);
                }
            }
            STATE_WALK_BACK => {
                self.physics.vel.x = -WALK_BACK_VEL * facing_sign;
                if up {
                    self.change_state(STATE_JUMP_START);
                } else if !back {
                    self.change_state(STATE_IDLE);
                }
            }
            STATE_CROUCH => {
                self.physics.vel.x = 0.0;
                if !down {
                    self.change_state(STATE_IDLE);
                }
            }
            STATE_JUMP_START => {
                self.physics.vel.x = 0.0;
                if self.state_time >= JUMP_START_TICKS {
                    // Set jump velocity and transition to airborne
                    self.physics.vel.y = JUMP_VEL_Y;
                    self.physics.apply_gravity(GRAVITY);

                    // Horizontal jump velocity based on held direction
                    if forward {
                        self.physics.vel.x = JUMP_FWD_VEL_X * facing_sign;
                    } else if back {
                        self.physics.vel.x = -JUMP_FWD_VEL_X * facing_sign;
                    }

                    self.change_state(STATE_AIRBORNE);
                }
            }
            STATE_AIRBORNE => {
                // Gravity is already applied via accel
                if self.physics.on_ground() && self.state_time > 0 {
                    self.physics.land();
                    self.change_state(STATE_LANDING);
                }
            }
            STATE_LANDING => {
                self.physics.vel.x = 0.0;
                if self.state_time >= LANDING_TICKS {
                    self.change_state(STATE_IDLE);
                }
            }
            _ => {
                // Unknown state — return to idle
                self.change_state(STATE_IDLE);
            }
        }

        // Physics step
        self.physics.step();

        // Animation tick
        self.anim.tick();

        // Increment state time
        self.state_time += 1;
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
                tracing::warn!("Failed to load palette {pal_idx} for sprite {sprite_id}: {e}");
                return None;
            }
        };

        if width == 0 || height == 0 {
            tracing::warn!("Sprite {sprite_id} has zero dimensions ({width}x{height})");
            return None;
        }

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

/// Maps a common state number to the expected MUGEN animation action number.
fn state_to_action(state: i32) -> i32 {
    match state {
        STATE_IDLE => 0,
        STATE_WALK_FWD | STATE_WALK_BACK => 20,
        STATE_CROUCH => 10,
        STATE_JUMP_START => 40,
        STATE_AIRBORNE => 41,
        STATE_LANDING => 47,
        _ => 0,
    }
}

/// Build an `InputState` from the current SDL2 keyboard state.
fn poll_input_state(keyboard: &sdl2::keyboard::KeyboardState<'_>) -> InputState {
    let mut state = InputState::default();

    // Directions: W/Up, S/Down, A/Left, D/Right
    state.direction.up =
        keyboard.is_scancode_pressed(Scancode::W) || keyboard.is_scancode_pressed(Scancode::Up);
    state.direction.down =
        keyboard.is_scancode_pressed(Scancode::S) || keyboard.is_scancode_pressed(Scancode::Down);
    state.direction.left =
        keyboard.is_scancode_pressed(Scancode::A) || keyboard.is_scancode_pressed(Scancode::Left);
    state.direction.right =
        keyboard.is_scancode_pressed(Scancode::D) || keyboard.is_scancode_pressed(Scancode::Right);

    // Buttons: U=a, I=b, O=c, J=x, K=y, L=z, Enter=start
    state.set_button(
        fp_input::Button::A,
        keyboard.is_scancode_pressed(Scancode::U),
    );
    state.set_button(
        fp_input::Button::B,
        keyboard.is_scancode_pressed(Scancode::I),
    );
    state.set_button(
        fp_input::Button::C,
        keyboard.is_scancode_pressed(Scancode::O),
    );
    state.set_button(
        fp_input::Button::X,
        keyboard.is_scancode_pressed(Scancode::J),
    );
    state.set_button(
        fp_input::Button::Y,
        keyboard.is_scancode_pressed(Scancode::K),
    );
    state.set_button(
        fp_input::Button::Z,
        keyboard.is_scancode_pressed(Scancode::L),
    );
    state.set_button(
        fp_input::Button::Start,
        keyboard.is_scancode_pressed(Scancode::Return),
    );

    state
}

/// Map AIR blend mode to renderer blend mode + alpha.
fn map_blend_mode(
    air_blend: &fp_formats::air::BlendMode,
) -> (fp_render::BlendMode, f32) {
    match air_blend {
        fp_formats::air::BlendMode::Normal => (fp_render::BlendMode::Normal, 1.0),
        fp_formats::air::BlendMode::Additive => (fp_render::BlendMode::Additive, 1.0),
        fp_formats::air::BlendMode::AdditiveAlpha(a) => {
            (fp_render::BlendMode::Additive, *a as f32 / 256.0)
        }
        fp_formats::air::BlendMode::Subtractive => (fp_render::BlendMode::Subtractive, 1.0),
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

    enum Mode {
        Playable(Box<PlayableCharacter>),
        Static(SpriteTexture, PaletteTexture),
        TestPattern(SpriteTexture, PaletteTexture),
    }

    let mut mode = match args.len() {
        4 => {
            // fp-app <sff> <air> <cmd> — playable character with commands
            let sff_path = Path::new(&args[1]);
            let air_path = Path::new(&args[2]);
            let cmd_path = Path::new(&args[3]);
            tracing::info!("Loading SFF: {}", sff_path.display());
            tracing::info!("Loading AIR: {}", air_path.display());
            tracing::info!("Loading CMD: {}", cmd_path.display());

            let sff = SffFile::load(sff_path)?;
            let air = AirFile::load(air_path)?;
            let cmd = CmdFile::load(cmd_path)?;

            tracing::info!(
                "SFF: {} sprites, {} palettes | AIR: {} actions | CMD: {} commands",
                sff.sprites.len(),
                sff.palettes.len(),
                air.actions.len(),
                cmd.commands.len(),
            );

            Mode::Playable(Box::new(PlayableCharacter::new(sff, air, Some(cmd))))
        }
        3 => {
            // fp-app <sff> <air> — playable character (no CMD)
            let sff_path = Path::new(&args[1]);
            let air_path = Path::new(&args[2]);
            tracing::info!("Loading SFF: {}", sff_path.display());
            tracing::info!("Loading AIR: {}", air_path.display());

            let sff = SffFile::load(sff_path)?;
            let air = AirFile::load(air_path)?;

            tracing::info!(
                "SFF: {} sprites, {} palettes | AIR: {} actions",
                sff.sprites.len(),
                sff.palettes.len(),
                air.actions.len(),
            );

            Mode::Playable(Box::new(PlayableCharacter::new(sff, air, None)))
        }
        2 => {
            // fp-app <sff> — static first sprite
            let (s, p) = load_sff_sprite(&renderer, Path::new(&args[1]))?;
            Mode::Static(s, p)
        }
        _ => {
            tracing::info!("No files provided; showing test pattern");
            tracing::info!("Usage: fp-app <file.sff> <file.air> [file.cmd]");
            let (s, p) = generate_test_pattern(&renderer);
            Mode::TestPattern(s, p)
        }
    };

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
                Mode::Playable(ref mut pc) => {
                    // 1. Build InputState from SDL2 keyboard
                    let keyboard = event_pump.keyboard_state();
                    let input = poll_input_state(&keyboard);

                    // 2. Push into InputBuffer
                    pc.input_buffer.push(input);

                    // 3. Run CommandMatcher (if CMD loaded)
                    if let Some(ref mut matcher) = pc.command_matcher {
                        matcher.check_commands(&pc.input_buffer, pc.facing_right);
                    }

                    // 4-6. State machine + physics + animation
                    pc.tick();
                }
                Mode::Static(..) | Mode::TestPattern(..) => {}
            }
            accumulator -= TICK_DURATION;
        }

        // Ensure the current animation frame's sprite is cached before rendering
        if let Mode::Playable(ref mut pc) = mode {
            let sprite_id = pc.anim.current_frame().sprite;
            pc.get_or_create_sprite(sprite_id, &renderer);
        }

        // Render
        let mut frame = renderer.begin_frame()?;
        frame.clear(0.1, 0.1, 0.15);

        let (win_w, win_h) = window.size();

        match mode {
            Mode::Playable(ref pc) => {
                let anim_frame = pc.anim.current_frame();
                let sprite_id = anim_frame.sprite;

                if let Some(cached) = pc.sprite_cache.get(&sprite_id) {
                    let center_x = win_w as f32 / 2.0;
                    let ground_y = win_h as f32 * 0.7;

                    // screen_x = center + physics.pos.x - axis + anim offset
                    let draw_x = center_x + pc.physics.pos.x
                        - cached.axis_x as f32
                        + anim_frame.offset.x as f32;
                    // screen_y = ground_line + physics.pos.y - axis + anim offset
                    let draw_y = ground_y + pc.physics.pos.y
                        - cached.axis_y as f32
                        + anim_frame.offset.y as f32;

                    let (render_blend, alpha) = map_blend_mode(&anim_frame.blend);

                    // Flip horizontally when facing left
                    let flip_h = if pc.facing_right {
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

    let sprite = sff.sprites.first().ok_or_else(|| {
        fp_core::FpError::not_found("sprite", "SFF file contains no sprites")
    })?;

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

    /// Create a minimal AIR file with standard KFM actions for testing.
    fn make_test_air() -> AirFile {
        use fp_core::Vec2;
        use fp_formats::air::{AnimFrame, BlendMode};

        let make_frame = |group: u16, image: u16| AnimFrame {
            sprite: SpriteId::new(group, image),
            offset: Vec2::new(0i16, 0i16),
            ticks: 5,
            flip_h: false,
            flip_v: false,
            blend: BlendMode::Normal,
            clsn1: vec![],
            clsn2: vec![],
        };

        let actions: HashMap<i32, AnimAction> = [
            (0, AnimAction { action_number: 0, frames: vec![make_frame(0, 0)], loopstart: 0 }),
            (10, AnimAction { action_number: 10, frames: vec![make_frame(10, 0)], loopstart: 0 }),
            (20, AnimAction { action_number: 20, frames: vec![make_frame(20, 0)], loopstart: 0 }),
            (40, AnimAction { action_number: 40, frames: vec![make_frame(40, 0)], loopstart: 0 }),
            (41, AnimAction { action_number: 41, frames: vec![make_frame(41, 0)], loopstart: 0 }),
            (47, AnimAction { action_number: 47, frames: vec![make_frame(47, 0)], loopstart: 0 }),
        ]
        .into_iter()
        .collect();

        AirFile { actions }
    }

    /// Build a minimal valid SFF v2 binary with 0 sprites and 0 palettes.
    fn make_empty_sff() -> SffFile {
        let mut buf = vec![0u8; 512];
        buf[0..12].copy_from_slice(b"ElecbyteSpr\0");
        buf[12] = 0; // minor3
        buf[13] = 0; // minor2
        buf[14] = 1; // minor1
        buf[15] = 2; // major
        // 0 groups, 0 sprites
        buf[36..40].copy_from_slice(&512u32.to_le_bytes()); // sprite_offset
        buf[44..48].copy_from_slice(&512u32.to_le_bytes()); // palette_offset
        buf[52..56].copy_from_slice(&512u32.to_le_bytes()); // ldata_offset
        buf[60..64].copy_from_slice(&512u32.to_le_bytes()); // tdata_offset
        SffFile::from_bytes(&buf).unwrap()
    }

    /// Create a PlayableCharacter with minimal SFF (no actual sprites needed for state tests).
    fn make_test_char() -> PlayableCharacter {
        let sff = make_empty_sff();
        let air = make_test_air();
        PlayableCharacter::new(sff, air, None)
    }

    fn push_input(pc: &mut PlayableCharacter, input: InputState) {
        pc.input_buffer.push(input);
        pc.tick();
    }

    fn input_with_dir(up: bool, down: bool, left: bool, right: bool) -> InputState {
        InputState {
            direction: fp_input::Direction {
                up,
                down,
                left,
                right,
            },
            ..Default::default()
        }
    }

    #[test]
    fn idle_to_walk_forward() {
        let mut pc = make_test_char();
        assert_eq!(pc.state, STATE_IDLE);

        // Press right (forward when facing right)
        push_input(&mut pc, input_with_dir(false, false, false, true));
        assert_eq!(pc.state, STATE_WALK_FWD);
    }

    #[test]
    fn walk_to_idle() {
        let mut pc = make_test_char();
        // Start walking
        push_input(&mut pc, input_with_dir(false, false, false, true));
        assert_eq!(pc.state, STATE_WALK_FWD);

        // Release direction
        push_input(&mut pc, InputState::default());
        assert_eq!(pc.state, STATE_IDLE);
    }

    #[test]
    fn idle_to_jump() {
        let mut pc = make_test_char();
        push_input(&mut pc, input_with_dir(true, false, false, false));
        assert_eq!(pc.state, STATE_JUMP_START);
    }

    #[test]
    fn jump_start_to_airborne() {
        let mut pc = make_test_char();
        // Enter jump start
        push_input(&mut pc, input_with_dir(true, false, false, false));
        assert_eq!(pc.state, STATE_JUMP_START);

        // Tick through jump start duration
        let up_input = input_with_dir(true, false, false, false);
        for _ in 0..JUMP_START_TICKS {
            push_input(&mut pc, up_input);
        }
        assert_eq!(pc.state, STATE_AIRBORNE);
    }

    #[test]
    fn airborne_to_landing() {
        let mut pc = make_test_char();
        // Get into airborne state
        push_input(&mut pc, input_with_dir(true, false, false, false));
        let up_input = input_with_dir(true, false, false, false);
        for _ in 0..JUMP_START_TICKS {
            push_input(&mut pc, up_input);
        }
        assert_eq!(pc.state, STATE_AIRBORNE);

        // Tick until landing
        let neutral = InputState::default();
        for _ in 0..200 {
            push_input(&mut pc, neutral);
            if pc.state == STATE_LANDING {
                break;
            }
        }
        assert_eq!(pc.state, STATE_LANDING);
    }

    #[test]
    fn landing_to_idle() {
        let mut pc = make_test_char();
        // Get to landing state
        push_input(&mut pc, input_with_dir(true, false, false, false));
        let up_input = input_with_dir(true, false, false, false);
        for _ in 0..JUMP_START_TICKS {
            push_input(&mut pc, up_input);
        }
        let neutral = InputState::default();
        for _ in 0..200 {
            push_input(&mut pc, neutral);
            if pc.state == STATE_LANDING {
                break;
            }
        }
        assert_eq!(pc.state, STATE_LANDING);

        // Tick through landing duration
        for _ in 0..LANDING_TICKS + 1 {
            push_input(&mut pc, neutral);
        }
        assert_eq!(pc.state, STATE_IDLE);
    }

    #[test]
    fn walk_to_jump() {
        let mut pc = make_test_char();
        // Walking forward
        push_input(&mut pc, input_with_dir(false, false, false, true));
        assert_eq!(pc.state, STATE_WALK_FWD);

        // Press up while walking
        push_input(&mut pc, input_with_dir(true, false, false, true));
        assert_eq!(pc.state, STATE_JUMP_START);
    }

    #[test]
    fn idle_to_crouch() {
        let mut pc = make_test_char();
        push_input(&mut pc, input_with_dir(false, true, false, false));
        assert_eq!(pc.state, STATE_CROUCH);
    }

    #[test]
    fn crouch_to_idle() {
        let mut pc = make_test_char();
        // Enter crouch
        push_input(&mut pc, input_with_dir(false, true, false, false));
        assert_eq!(pc.state, STATE_CROUCH);

        // Release down
        push_input(&mut pc, InputState::default());
        assert_eq!(pc.state, STATE_IDLE);
    }
}
