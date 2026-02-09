//! Fighters Paradise — a modern MUGEN engine reimplementation in Rust.
//!
//! This is the application entry point. It initializes the SDL2 window,
//! sets up the wgpu rendering pipeline, and runs the main 60Hz game loop.
//!
//! # Usage
//!
//! ```text
//! cargo run -p fp-app [path/to/file.sff]
//! ```
//!
//! If an SFF file path is provided, the first sprite (group 0, image 0) is
//! displayed with its palette. Otherwise a procedurally generated test pattern
//! (checkerboard) is shown to verify the rendering pipeline works.

use std::path::Path;
use std::time::{Duration, Instant};

use fp_render::{PaletteTexture, Renderer, SpriteDrawParams, SpriteTexture};
use sdl2::event::Event;
use sdl2::keyboard::Keycode;

/// Window width in pixels.
const WINDOW_WIDTH: u32 = 640;
/// Window height in pixels.
const WINDOW_HEIGHT: u32 = 480;
/// Fixed timestep duration: 1/60th of a second (≈16.667ms).
const TICK_DURATION: Duration = Duration::from_nanos(16_666_667);

fn main() {
    // Initialise structured logging
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

    // --- Load sprite content ---
    let sff_path = std::env::args().nth(1);
    let (sprite_tex, palette_tex) = if let Some(ref path) = sff_path {
        load_sff_sprite(&renderer, Path::new(path))?
    } else {
        tracing::info!("No SFF file provided; showing test pattern");
        generate_test_pattern(&renderer)
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
            // Game logic tick would go here
            accumulator -= TICK_DURATION;
        }

        // Render
        let mut frame = renderer.begin_frame()?;
        frame.clear(0.1, 0.1, 0.15); // dark blue-gray background

        // Center the sprite on screen
        let (win_w, win_h) = window.size();
        let params = SpriteDrawParams {
            x: (win_w as f32 - sprite_tex.width as f32) / 2.0,
            y: (win_h as f32 - sprite_tex.height as f32) / 2.0,
            ..Default::default()
        };
        frame.draw_sprite(&sprite_tex, &palette_tex, &params);

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
    let sff = fp_formats::sff::SffFile::load(path)?;

    tracing::info!(
        "SFF loaded: {} sprites, {} palettes",
        sff.sprites.len(),
        sff.palettes.len()
    );

    // Use the first sprite
    let sprite = sff.sprites.first().ok_or_else(|| {
        fp_core::FpError::not_found("sprite", "SFF file contains no sprites")
    })?;

    let sprite_index = 0;
    let pixels = sff.decode_sprite(sprite_index)?;
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
            // Use palette indices 1 and 2 (index 0 is transparent)
            pixels[(y * size + x) as usize] = if checker == 0 { 1 } else { 2 };
        }
    }

    // Build a simple palette: index 0 = transparent black, 1 = white, 2 = dark gray
    let mut palette = [0u8; 1024];
    // Index 0: transparent (R=0, G=0, B=0, A=0) — already zero
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
    let palette_tex =
        PaletteTexture::new(renderer.device(), renderer.queue(), &palette);

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
