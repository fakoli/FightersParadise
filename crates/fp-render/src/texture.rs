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
