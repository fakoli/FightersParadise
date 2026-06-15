//! Texture atlas for batching multiple indexed sprites into a single GPU texture.
//!
//! Uses a simple row-based (shelf) packing algorithm. Sprites are placed
//! left-to-right in the current row; when a sprite doesn't fit horizontally,
//! a new row is started. The atlas uses `R8Unorm` format (palette-indexed),
//! matching the engine's palette-lookup rendering pipeline.

use fp_core::SpriteId;

/// UV rectangle within the atlas (normalized 0.0–1.0 coordinates).
#[derive(Debug, Clone, Copy)]
pub struct AtlasRegion {
    /// Left edge U coordinate.
    pub u_min: f32,
    /// Top edge V coordinate.
    pub v_min: f32,
    /// Right edge U coordinate.
    pub u_max: f32,
    /// Bottom edge V coordinate.
    pub v_max: f32,
    /// Sprite width in pixels.
    pub width: u32,
    /// Sprite height in pixels.
    pub height: u32,
}

/// A texture atlas that packs multiple indexed sprites into one GPU texture.
///
/// Sprites are packed using a shelf (row-based) algorithm: each row has a fixed
/// height equal to the tallest sprite placed in it. When a sprite doesn't fit
/// in the current row, a new shelf is started below.
pub struct TextureAtlas {
    /// The backing GPU texture (`R8Unorm` format).
    pub texture: wgpu::Texture,
    /// Texture view for shader binding.
    pub view: wgpu::TextureView,
    /// Nearest-neighbor sampler (pixel art).
    pub sampler: wgpu::Sampler,
    width: u32,
    height: u32,
    regions: Vec<(SpriteId, AtlasRegion)>,
    current_x: u32,
    current_y: u32,
    row_height: u32,
}

impl TextureAtlas {
    /// Creates a new empty atlas with the given dimensions.
    ///
    /// The atlas texture is created with `R8Unorm` format and supports both
    /// texture binding and copy-destination usage.
    pub fn new(device: &wgpu::Device, width: u32, height: u32) -> Self {
        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("atlas_texture"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("atlas_sampler"),
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
            regions: Vec::new(),
            current_x: 0,
            current_y: 0,
            row_height: 0,
        }
    }

    /// Packs a sprite into the atlas, uploading its pixel data to the GPU.
    ///
    /// Returns the [`AtlasRegion`] with normalized UV coordinates, or `None`
    /// if the sprite doesn't fit in the remaining atlas space.
    ///
    /// `data` must contain exactly `width * height` bytes of palette-indexed pixels.
    pub fn pack(
        &mut self,
        queue: &wgpu::Queue,
        id: SpriteId,
        width: u32,
        height: u32,
        data: &[u8],
    ) -> Option<AtlasRegion> {
        if width == 0 || height == 0 {
            return None;
        }

        // Try current row
        if self.current_x + width > self.width {
            // Start a new row
            self.current_y += self.row_height;
            self.current_x = 0;
            self.row_height = 0;
        }

        // Check vertical space
        if self.current_y + height > self.height {
            return None;
        }

        // Check horizontal space (sprite wider than atlas)
        if width > self.width {
            return None;
        }

        let x = self.current_x;
        let y = self.current_y;

        // Upload pixel data to the sub-region of the atlas texture
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );

        let region = AtlasRegion {
            u_min: x as f32 / self.width as f32,
            v_min: y as f32 / self.height as f32,
            u_max: (x + width) as f32 / self.width as f32,
            v_max: (y + height) as f32 / self.height as f32,
            width,
            height,
        };

        self.regions.push((id, region));
        self.current_x += width;
        if height > self.row_height {
            self.row_height = height;
        }

        Some(region)
    }

    /// Looks up a previously packed sprite's region by its [`SpriteId`].
    pub fn get(&self, id: SpriteId) -> Option<&AtlasRegion> {
        self.regions
            .iter()
            .find(|(sid, _)| *sid == id)
            .map(|(_, region)| region)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates a wgpu device and queue for testing (requires a GPU or software adapter).
    fn create_test_device() -> Option<(wgpu::Device, wgpu::Queue)> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });

        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))?;

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("test_device"),
                ..Default::default()
            },
            None,
        ))
        .ok()?;

        Some((device, queue))
    }

    #[test]
    fn pack_single_sprite() {
        let Some((device, queue)) = create_test_device() else {
            eprintln!("Skipping GPU test: no adapter available");
            return;
        };

        let mut atlas = TextureAtlas::new(&device, 256, 256);
        let id = SpriteId::new(0, 0);
        let data = vec![0u8; 32 * 32];

        let region = atlas.pack(&queue, id, 32, 32, &data);
        assert!(region.is_some());

        let region = region.unwrap();
        assert!((region.u_min - 0.0).abs() < f32::EPSILON);
        assert!((region.v_min - 0.0).abs() < f32::EPSILON);
        assert!((region.u_max - 32.0 / 256.0).abs() < f32::EPSILON);
        assert!((region.v_max - 32.0 / 256.0).abs() < f32::EPSILON);
        assert_eq!(region.width, 32);
        assert_eq!(region.height, 32);
    }

    #[test]
    fn pack_multiple_sprites_same_row() {
        let Some((device, queue)) = create_test_device() else {
            eprintln!("Skipping GPU test: no adapter available");
            return;
        };

        let mut atlas = TextureAtlas::new(&device, 256, 256);

        let r1 = atlas
            .pack(&queue, SpriteId::new(0, 0), 64, 32, &[0u8; 64 * 32])
            .unwrap();
        let r2 = atlas
            .pack(&queue, SpriteId::new(0, 1), 64, 32, &[0u8; 64 * 32])
            .unwrap();

        // First sprite at x=0, second at x=64
        assert!((r1.u_min - 0.0).abs() < f32::EPSILON);
        assert!((r2.u_min - 64.0 / 256.0).abs() < f32::EPSILON);
        // Both on the same row
        assert!((r1.v_min - r2.v_min).abs() < f32::EPSILON);
    }

    #[test]
    fn pack_overflow_starts_new_shelf() {
        let Some((device, queue)) = create_test_device() else {
            eprintln!("Skipping GPU test: no adapter available");
            return;
        };

        let mut atlas = TextureAtlas::new(&device, 128, 128);

        // Fill the first row with a 100-wide sprite
        atlas
            .pack(&queue, SpriteId::new(0, 0), 100, 30, &[0u8; 100 * 30])
            .unwrap();

        // This 50-wide sprite won't fit (100+50 > 128), so it starts a new row
        let r2 = atlas
            .pack(&queue, SpriteId::new(0, 1), 50, 20, &[0u8; 50 * 20])
            .unwrap();

        // New row starts at y = 30 (height of the first shelf)
        assert!((r2.v_min - 30.0 / 128.0).abs() < f32::EPSILON);
        assert!((r2.u_min - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn pack_atlas_full_returns_none() {
        let Some((device, queue)) = create_test_device() else {
            eprintln!("Skipping GPU test: no adapter available");
            return;
        };

        let mut atlas = TextureAtlas::new(&device, 64, 64);

        // Fill with a sprite taking the entire atlas
        atlas
            .pack(&queue, SpriteId::new(0, 0), 64, 64, &[0u8; 64 * 64])
            .unwrap();

        // No room for another sprite
        let result = atlas.pack(&queue, SpriteId::new(0, 1), 1, 1, &[0u8; 1]);
        assert!(result.is_none());
    }

    #[test]
    fn get_returns_correct_region() {
        let Some((device, queue)) = create_test_device() else {
            eprintln!("Skipping GPU test: no adapter available");
            return;
        };

        let mut atlas = TextureAtlas::new(&device, 256, 256);
        let id = SpriteId::new(5, 3);
        atlas.pack(&queue, id, 16, 16, &[0u8; 16 * 16]).unwrap();

        assert!(atlas.get(id).is_some());
        assert!(atlas.get(SpriteId::new(99, 99)).is_none());
    }

    #[test]
    fn pack_zero_size_returns_none() {
        let Some((device, queue)) = create_test_device() else {
            eprintln!("Skipping GPU test: no adapter available");
            return;
        };

        let atlas = &mut TextureAtlas::new(&device, 256, 256);
        assert!(atlas
            .pack(&queue, SpriteId::new(0, 0), 0, 10, &[])
            .is_none());
        assert!(atlas
            .pack(&queue, SpriteId::new(0, 0), 10, 0, &[])
            .is_none());
    }
}
