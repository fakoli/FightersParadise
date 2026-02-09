//! Fighters Paradise — a modern MUGEN engine reimplementation in Rust.
//!
//! This is the application entry point. It initializes the SDL2 window,
//! sets up the wgpu rendering pipeline, and runs the main 60Hz game loop.
//!
//! # Usage
//!
//! ```text
//! cargo run -p fp-app -- <file.sff> <file.air>    # animate from SFF+AIR
//! cargo run -p fp-app -- <file.sff>                # show first sprite
//! cargo run -p fp-app                              # checkerboard test pattern
//! ```
//!
//! When both SFF and AIR files are provided, the character's animations play
//! in a loop. Use Left/Right arrows to cycle animation actions and Space to
//! restart the current action.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use fp_core::SpriteId;
use fp_formats::air::{AirFile, AnimAction};
use fp_formats::sff::SffFile;
use fp_render::{AnimController, PaletteTexture, Renderer, SpriteDrawParams, SpriteTexture};
use sdl2::event::Event;
use sdl2::keyboard::Keycode;

/// Window width in pixels.
const WINDOW_WIDTH: u32 = 640;
/// Window height in pixels.
const WINDOW_HEIGHT: u32 = 480;
/// Fixed timestep duration: 1/60th of a second (~16.667ms).
const TICK_DURATION: Duration = Duration::from_nanos(16_666_667);

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

/// Holds the loaded character data for animation playback.
struct CharacterData {
    sff: SffFile,
    air: AirFile,
    /// Sorted list of available action numbers for cycling.
    action_list: Vec<i32>,
    /// Current index into `action_list`.
    action_index: usize,
    /// Animation controller driving playback.
    anim: AnimController,
    /// Texture cache keyed by SpriteId.
    sprite_cache: HashMap<SpriteId, CachedSprite>,
}

impl CharacterData {
    fn new(sff: SffFile, air: AirFile) -> Self {
        let mut action_list: Vec<i32> = air.actions.keys().copied().collect();
        action_list.sort();

        let first_action_num = action_list.first().copied().unwrap_or(0);
        let first_action = air
            .action(first_action_num)
            .cloned()
            .unwrap_or(AnimAction {
                action_number: 0,
                frames: vec![],
                loopstart: 0,
            });

        tracing::info!(
            "Loaded {} actions: {:?}",
            action_list.len(),
            &action_list[..action_list.len().min(20)]
        );
        tracing::info!("Starting with action {first_action_num}");

        Self {
            sff,
            air,
            action_list,
            action_index: 0,
            anim: AnimController::new(first_action),
            sprite_cache: HashMap::new(),
        }
    }

    /// Switch to the next action in the sorted list.
    fn next_action(&mut self) {
        if self.action_list.is_empty() {
            return;
        }
        self.action_index = (self.action_index + 1) % self.action_list.len();
        self.switch_to_current_action();
    }

    /// Switch to the previous action in the sorted list.
    fn prev_action(&mut self) {
        if self.action_list.is_empty() {
            return;
        }
        self.action_index = if self.action_index == 0 {
            self.action_list.len() - 1
        } else {
            self.action_index - 1
        };
        self.switch_to_current_action();
    }

    /// Restart the current action from frame 0.
    fn restart_action(&mut self) {
        self.switch_to_current_action();
    }

    fn switch_to_current_action(&mut self) {
        let action_num = self.action_list[self.action_index];
        if let Some(action) = self.air.action(action_num) {
            tracing::info!(
                "Switched to action {} ({} frames)",
                action_num,
                action.frames.len()
            );
            self.anim.set_action(action.clone());
        }
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

        // Find sprite index in SFF
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

        // Decode pixel data
        let pixels = match self.sff.decode_sprite(index) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("Failed to decode sprite {sprite_id}: {e}");
                return None;
            }
        };

        // Get palette
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
        Animated(Box<CharacterData>),
        Static(SpriteTexture, PaletteTexture),
        TestPattern(SpriteTexture, PaletteTexture),
    }

    let mut mode = match args.len() {
        3 => {
            // fp-app <sff> <air>
            let sff_path = Path::new(&args[1]);
            let air_path = Path::new(&args[2]);
            tracing::info!("Loading SFF: {}", sff_path.display());
            tracing::info!("Loading AIR: {}", air_path.display());

            let sff = SffFile::load(sff_path)?;
            tracing::info!(
                "SFF loaded: {} sprites, {} palettes",
                sff.sprites.len(),
                sff.palettes.len()
            );

            let air = AirFile::load(air_path)?;
            Mode::Animated(Box::new(CharacterData::new(sff, air)))
        }
        2 => {
            // fp-app <sff> — static first sprite
            let (s, p) = load_sff_sprite(&renderer, Path::new(&args[1]))?;
            Mode::Static(s, p)
        }
        _ => {
            tracing::info!("No files provided; showing test pattern");
            tracing::info!("Usage: fp-app <file.sff> <file.air>");
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
                Event::KeyDown {
                    keycode: Some(Keycode::Right),
                    repeat: false,
                    ..
                } => {
                    if let Mode::Animated(ref mut data) = mode {
                        data.next_action();
                    }
                }
                Event::KeyDown {
                    keycode: Some(Keycode::Left),
                    repeat: false,
                    ..
                } => {
                    if let Mode::Animated(ref mut data) = mode {
                        data.prev_action();
                    }
                }
                Event::KeyDown {
                    keycode: Some(Keycode::Space),
                    repeat: false,
                    ..
                } => {
                    if let Mode::Animated(ref mut data) = mode {
                        data.restart_action();
                    }
                }
                _ => {}
            }
        }

        // Fixed timestep accumulation
        let current = Instant::now();
        accumulator += current - previous;
        previous = current;

        while accumulator >= TICK_DURATION {
            if let Mode::Animated(ref mut data) = mode {
                data.anim.tick();
            }
            accumulator -= TICK_DURATION;
        }

        // Ensure the current animation frame's sprite is cached before borrowing
        // the renderer for the frame (avoids borrow conflict).
        if let Mode::Animated(ref mut data) = mode {
            let sprite_id = data.anim.current_frame().sprite;
            data.get_or_create_sprite(sprite_id, &renderer);
        }

        // Render
        let mut frame = renderer.begin_frame()?;
        frame.clear(0.1, 0.1, 0.15);

        let (win_w, win_h) = window.size();

        match mode {
            Mode::Animated(ref data) => {
                let anim_frame = data.anim.current_frame();
                let sprite_id = anim_frame.sprite;

                if let Some(cached) = data.sprite_cache.get(&sprite_id) {
                    let center_x = win_w as f32 / 2.0;
                    let center_y = win_h as f32 * 0.7; // ground line at 70%

                    let draw_x =
                        center_x - cached.axis_x as f32 + anim_frame.offset.x as f32;
                    let draw_y =
                        center_y - cached.axis_y as f32 + anim_frame.offset.y as f32;

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
