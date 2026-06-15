/// GPU texture wrapper for an indexed (8-bit) sprite image.
///
/// The texture format is `R8Unorm` — each pixel holds a palette index (0–255).
/// The actual color lookup happens in the palette shader.
pub struct SpriteTexture {
    /// The GPU texture.
    pub texture: wgpu::Texture,
    /// A view into the texture for binding.
    pub view: wgpu::TextureView,
    /// Nearest-neighbor sampler (pixel art should not be filtered).
    pub sampler: wgpu::Sampler,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl SpriteTexture {
    /// Uploads indexed pixel data to a new GPU texture.
    ///
    /// `data` must contain exactly `width * height` bytes, one palette index per pixel.
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, width: u32, height: u32, data: &[u8]) -> Self {
        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("sprite_texture"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width),
                rows_per_image: Some(height),
            },
            size,
        );

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("sprite_sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        Self {
            texture,
            view,
            sampler,
            width,
            height,
        }
    }
}

/// GPU texture wrapper for a 256-color RGBA palette.
///
/// Stored as a 256×1 `Rgba8UnormSrgb` texture. The fragment shader samples
/// this with the palette index as the U coordinate to look up the final color.
pub struct PaletteTexture {
    /// The GPU texture.
    pub texture: wgpu::Texture,
    /// A view into the texture for binding.
    pub view: wgpu::TextureView,
    /// Linear sampler for palette lookup.
    pub sampler: wgpu::Sampler,
}

impl PaletteTexture {
    /// Uploads a palette texture, choosing an external `.act` override when one
    /// is supplied and otherwise the sprite's embedded palette.
    ///
    /// This is the runtime `.act` costume-swap path: MUGEN characters may ship up
    /// to twelve alternate palettes (`pal1`..`pal12`), and selecting one renders
    /// the character with that palette instead of the SFF-embedded one. Since the
    /// GPU palette lookup is a plain 256×1 RGBA texture, swapping costumes is just
    /// swapping which 1024-byte buffer is uploaded — no sprite re-upload.
    ///
    /// - `embedded` is the SFF-embedded palette (the default, e.g. what
    ///   `fp_formats::sff::SffFile::palette` returns).
    /// - `override_rgba` is the selected `.act` override's RGBA, or `None` to use
    ///   the embedded palette.
    ///
    /// When `override_rgba` is `None` this is **byte-identical** to
    /// [`PaletteTexture::new`] called on `embedded`, so the default (no-override)
    /// render path is unchanged. Resolve a runtime selection to `override_rgba`
    /// with `fp_character::LoadedCharacter::override_palette`.
    pub fn from_override(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        embedded: &[u8; 1024],
        override_rgba: Option<&[u8; 1024]>,
    ) -> Self {
        Self::new(device, queue, select_palette_bytes(embedded, override_rgba))
    }

    /// Uploads 256 RGBA entries (1024 bytes) to a new GPU texture.
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, data: &[u8; 1024]) -> Self {
        let size = wgpu::Extent3d {
            width: 256,
            height: 1,
            depth_or_array_layers: 1,
        };

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("palette_texture"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(256 * 4),
                rows_per_image: Some(1),
            },
            size,
        );

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("palette_sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        Self {
            texture,
            view,
            sampler,
        }
    }
}

/// Chooses the palette bytes to upload: the external `.act` override when
/// present, otherwise the SFF-embedded palette.
///
/// The pure selection logic behind [`PaletteTexture::from_override`], extracted
/// so it is unit-testable without a GPU device. Returns `embedded` unchanged
/// (same `&` reference) when `override_rgba` is `None`, guaranteeing the
/// no-override upload is byte-identical to the embedded-palette path.
fn select_palette_bytes<'a>(
    embedded: &'a [u8; 1024],
    override_rgba: Option<&'a [u8; 1024]>,
) -> &'a [u8; 1024] {
    override_rgba.unwrap_or(embedded)
}

/// GPU texture wrapper for a full-color **RGBA** image (e.g. a stage background).
///
/// Unlike [`SpriteTexture`] (palette-indexed `R8Unorm`, recolored in the shader),
/// this stores the colors directly as `Rgba8UnormSrgb` and is drawn by
/// [`RenderFrame::draw_image`](crate::RenderFrame::draw_image) with **linear**
/// filtering, so a background scaled to fill the window stays smooth rather than
/// blocky. No palette, no index-0 transparency, no PalFX.
pub struct ImageTexture {
    /// The GPU texture.
    pub texture: wgpu::Texture,
    /// A view into the texture for binding.
    pub view: wgpu::TextureView,
    /// Linear sampler (full-color art is scaled, so it should be filtered).
    pub sampler: wgpu::Sampler,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl ImageTexture {
    /// Uploads `width * height * 4` bytes of `RGBA` pixel data to a new GPU
    /// texture. `rgba` must be exactly that length (row-major, 4 bytes/pixel).
    #[must_use]
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Self {
        // The GPU upload below requires exactly `width*height*4` bytes; a short
        // slice would trip a wgpu validation error (an abort inside wgpu) rather
        // than fail gracefully. Catch a misuse in dev/test at zero release cost.
        debug_assert_eq!(
            rgba.len(),
            width as usize * height as usize * 4,
            "ImageTexture::new: rgba must be width*height*4 bytes"
        );
        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("image_texture"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(height),
            },
            size,
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("image_sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Self {
            texture,
            view,
            sampler,
            width,
            height,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_palette_bytes_uses_embedded_when_no_override() {
        let embedded = [7u8; 1024];
        let chosen = select_palette_bytes(&embedded, None);
        // No override → the embedded palette, byte-identical (same pointer even).
        assert!(std::ptr::eq(chosen, &embedded));
        assert_eq!(chosen, &embedded);
    }

    #[test]
    fn select_palette_bytes_uses_override_when_present() {
        let embedded = [7u8; 1024];
        let mut over = [7u8; 1024];
        // Make the override differ from the embedded palette at one index.
        over[4] = 200;
        let chosen = select_palette_bytes(&embedded, Some(&over));
        assert!(std::ptr::eq(chosen, &over));
        assert_ne!(chosen, &embedded);
        assert_eq!(chosen[4], 200);
    }

    /// Builds a representative SFF-embedded RGBA palette (256 × RGBA, index 0
    /// transparent) where every entry differs from the `.act` fixture below, so
    /// the override is observably *replacing* the embedded palette, not matching
    /// it by accident.
    fn embedded_palette() -> [u8; 1024] {
        let mut rgba = [0u8; 1024];
        for i in 0..256usize {
            let dst = i * 4;
            // A simple ramp distinct from the .act fixture's constant grey.
            rgba[dst] = i as u8; // R
            rgba[dst + 1] = 1; // G
            rgba[dst + 2] = 2; // B
            rgba[dst + 3] = if i == 0 { 0 } else { 255 }; // index 0 transparent
        }
        rgba
    }

    /// End-to-end runtime selection: a real `.act` palette (parsed by
    /// `fp_formats::act::ActPalette`) overrides the SFF-embedded palette, and the
    /// MUGEN index-0-transparency rule survives the swap. This is the non-gated,
    /// fully-synthetic assertion for the `.act`-overrides-embedded acceptance
    /// criterion — it exercises the same `select_palette_bytes` seam that
    /// `PaletteTexture::from_override` (the GPU upload) uses.
    #[test]
    fn act_palette_overrides_embedded_and_keeps_index0_transparent() {
        // A 768-byte .act: a constant opaque grey for every on-disk triple. The
        // ActPalette parser de-reverses it and forces index 0 transparent.
        let act_bytes = vec![90u8; 768];
        let act = fp_formats::act::ActPalette::from_bytes(&act_bytes)
            .expect("synthetic .act parses");

        let embedded = embedded_palette();

        // With the .act selected, the chosen bytes are the override, NOT the
        // embedded palette: the costume swap actually replaced the active palette.
        let chosen = select_palette_bytes(&embedded, Some(&act.rgba));
        assert!(std::ptr::eq(chosen, &act.rgba));
        assert_ne!(
            chosen, &embedded,
            ".act override must differ from the SFF-embedded palette"
        );

        // Index 0 transparency is preserved through the swap (alpha == 0), while
        // an opaque color index keeps alpha == 255 — the MUGEN convention holds
        // for the .act exactly as for the embedded palette.
        assert_eq!(chosen[3], 0, "palette index 0 must stay transparent");
        assert_eq!(chosen[4 + 3], 255, "index 1 must be opaque");
        // The override's color came from the .act, not the embedded ramp.
        assert_eq!(chosen[4], 90, "index 1 RGB must come from the .act");
        assert_ne!(
            chosen[4], embedded[4],
            ".act color must replace the embedded color at index 1"
        );

        // And the no-override path still falls back to the embedded palette,
        // byte-identical — costumeless characters render exactly as before.
        let fallback = select_palette_bytes(&embedded, None);
        assert!(std::ptr::eq(fallback, &embedded));
    }
}
