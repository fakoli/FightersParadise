/// A vertex for sprite quad rendering.
///
/// Each sprite is drawn as a textured quad (4 vertices, 6 indices).
/// The position is in screen-space pixels and the UV coordinates map
/// into the sprite texture.
#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct SpriteVertex {
    /// Screen-space position in pixels.
    pub position: [f32; 2],
    /// Texture coordinates (0.0–1.0).
    pub uv: [f32; 2],
    /// Per-vertex opacity multiplier (0.0–1.0).
    pub alpha: f32,
}

impl SpriteVertex {
    /// Returns the vertex buffer layout descriptor for the render pipeline.
    pub fn desc() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<SpriteVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                // position
                wgpu::VertexAttribute {
                    offset: 0,
                    shader_location: 0,
                    format: wgpu::VertexFormat::Float32x2,
                },
                // uv
                wgpu::VertexAttribute {
                    offset: std::mem::size_of::<[f32; 2]>() as wgpu::BufferAddress,
                    shader_location: 1,
                    format: wgpu::VertexFormat::Float32x2,
                },
                // alpha
                wgpu::VertexAttribute {
                    offset: std::mem::size_of::<[f32; 4]>() as wgpu::BufferAddress,
                    shader_location: 2,
                    format: wgpu::VertexFormat::Float32,
                },
            ],
        }
    }
}

/// A vertex for the unlit debug overlay (collision-box outlines/fills).
///
/// Carries a screen-space position and a per-vertex RGBA color; the debug
/// shader performs no texture lookup, so this is all the geometry it needs.
#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct DebugVertex {
    /// Screen-space position in pixels.
    pub position: [f32; 2],
    /// Per-vertex color, RGBA in the range 0.0–1.0.
    pub color: [f32; 4],
}

impl DebugVertex {
    /// Returns the vertex buffer layout descriptor for the debug pipeline.
    pub fn desc() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<DebugVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                // position
                wgpu::VertexAttribute {
                    offset: 0,
                    shader_location: 0,
                    format: wgpu::VertexFormat::Float32x2,
                },
                // color
                wgpu::VertexAttribute {
                    offset: std::mem::size_of::<[f32; 2]>() as wgpu::BufferAddress,
                    shader_location: 1,
                    format: wgpu::VertexFormat::Float32x4,
                },
            ],
        }
    }
}
