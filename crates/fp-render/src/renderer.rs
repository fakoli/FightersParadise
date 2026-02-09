use fp_core::{FpError, FpResult};
use wgpu::util::DeviceExt;

use crate::params::SpriteDrawParams;
use crate::texture::{PaletteTexture, SpriteTexture};
use crate::vertex::SpriteVertex;

/// Quad index data: two triangles forming a rectangle.
const QUAD_INDICES: [u16; 6] = [0, 1, 2, 0, 2, 3];

/// Builds an orthographic projection matrix for screen-space rendering.
///
/// Origin at top-left, Y increases downward, maps `(0..width, 0..height)` to
/// clip space `(-1..1, -1..1)`. Returns column-major `[f32; 16]` for wgpu.
fn ortho_projection(width: f32, height: f32) -> [f32; 16] {
    [
        2.0 / width, 0.0,           0.0, 0.0,
        0.0,        -2.0 / height,  0.0, 0.0,
        0.0,         0.0,           1.0, 0.0,
       -1.0,         1.0,           0.0, 1.0,
    ]
}

/// The wgpu-based sprite renderer for the Fighters Paradise engine.
///
/// Manages the GPU device, surface, render pipeline, and shared buffers.
/// Sprites are drawn one at a time via [`RenderFrame::draw_sprite`]; each
/// call uploads a fresh quad and issues a draw.
pub struct Renderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    surface_config: wgpu::SurfaceConfiguration,
    pipeline: wgpu::RenderPipeline,
    _uniform_bind_group_layout: wgpu::BindGroupLayout,
    texture_bind_group_layout: wgpu::BindGroupLayout,
    uniform_buffer: wgpu::Buffer,
    uniform_bind_group: wgpu::BindGroup,
    vertex_buffer: wgpu::Buffer,
    index_buffer: wgpu::Buffer,
}

impl Renderer {
    /// Initialises the renderer: requests an adapter, creates the device, configures
    /// the surface, builds the render pipeline and shared GPU resources.
    pub async fn new(
        instance: &wgpu::Instance,
        surface: wgpu::Surface<'static>,
        width: u32,
        height: u32,
    ) -> FpResult<Self> {
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .ok_or_else(|| FpError::Render("no suitable GPU adapter found".into()))?;

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("fp_device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                memory_hints: wgpu::MemoryHints::Performance,
            }, None)
            .await
            .map_err(|e| FpError::Render(format!("failed to create device: {e}")))?;

        let surface_caps = surface.get_capabilities(&adapter);
        let surface_format = surface_caps
            .formats
            .iter()
            .find(|f| f.is_srgb())
            .copied()
            .unwrap_or(surface_caps.formats[0]);

        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width,
            height,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: surface_caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &surface_config);

        // --- Bind group layouts ---

        let uniform_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("uniform_bind_group_layout"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });

        let texture_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("texture_bind_group_layout"),
                entries: &[
                    // sprite texture
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    // sprite sampler
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                    // palette texture
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    // palette sampler
                    wgpu::BindGroupLayoutEntry {
                        binding: 3,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });

        // --- Shader ---

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("palette_shader"),
            source: wgpu::ShaderSource::Wgsl(
                include_str!("shaders/palette.wgsl").into(),
            ),
        });

        // --- Pipeline ---

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("sprite_pipeline_layout"),
            bind_group_layouts: &[&uniform_bind_group_layout, &texture_bind_group_layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("sprite_pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[SpriteVertex::desc()],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        // --- Uniform buffer ---

        let projection = ortho_projection(width as f32, height as f32);
        let uniform_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("uniform_buffer"),
            contents: bytemuck::cast_slice(&projection),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let uniform_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("uniform_bind_group"),
            layout: &uniform_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            }],
        });

        // --- Vertex / index buffers (quad placeholder, overwritten each draw) ---

        let vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("sprite_vertex_buffer"),
            size: (std::mem::size_of::<SpriteVertex>() * 4) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let index_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("sprite_index_buffer"),
            contents: bytemuck::cast_slice(&QUAD_INDICES),
            usage: wgpu::BufferUsages::INDEX,
        });

        Ok(Self {
            device,
            queue,
            surface,
            surface_config,
            pipeline,
            _uniform_bind_group_layout: uniform_bind_group_layout,
            texture_bind_group_layout,
            uniform_buffer,
            uniform_bind_group,
            vertex_buffer,
            index_buffer,
        })
    }

    /// Reconfigure the surface and projection after a window resize.
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.surface_config.width = width;
        self.surface_config.height = height;
        self.surface.configure(&self.device, &self.surface_config);

        let projection = ortho_projection(width as f32, height as f32);
        self.queue
            .write_buffer(&self.uniform_buffer, 0, bytemuck::cast_slice(&projection));
    }

    /// Acquire the next surface texture and begin recording draw commands.
    pub fn begin_frame(&mut self) -> FpResult<RenderFrame<'_>> {
        let output = self
            .surface
            .get_current_texture()
            .map_err(|e| FpError::Render(format!("failed to acquire surface texture: {e}")))?;

        let view = output
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame_encoder"),
            });

        Ok(RenderFrame {
            renderer: self,
            output,
            view,
            encoder,
        })
    }

    /// Provides read access to the GPU device (e.g. for creating textures).
    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    /// Provides read access to the GPU queue (e.g. for uploading texture data).
    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }
}

/// An in-progress frame that accumulates draw commands.
///
/// Created by [`Renderer::begin_frame`]. Call [`draw_sprite`](Self::draw_sprite)
/// for each sprite, then [`finish`](Self::finish) to submit and present.
pub struct RenderFrame<'a> {
    renderer: &'a Renderer,
    output: wgpu::SurfaceTexture,
    view: wgpu::TextureView,
    encoder: wgpu::CommandEncoder,
}

impl RenderFrame<'_> {
    /// Draw a palette-indexed sprite with the given parameters.
    pub fn draw_sprite(
        &mut self,
        texture: &SpriteTexture,
        palette: &PaletteTexture,
        params: &SpriteDrawParams,
    ) {
        let w = texture.width as f32 * params.scale_x;
        let h = texture.height as f32 * params.scale_y;

        let (u_left, u_right) = if params.flip_h { (1.0, 0.0) } else { (0.0, 1.0) };
        let (v_top, v_bottom) = if params.flip_v { (1.0, 0.0) } else { (0.0, 1.0) };

        let vertices = [
            SpriteVertex { position: [params.x,     params.y    ], uv: [u_left,  v_top   ] },
            SpriteVertex { position: [params.x + w, params.y    ], uv: [u_right, v_top   ] },
            SpriteVertex { position: [params.x + w, params.y + h], uv: [u_right, v_bottom] },
            SpriteVertex { position: [params.x,     params.y + h], uv: [u_left,  v_bottom] },
        ];

        self.renderer.queue.write_buffer(
            &self.renderer.vertex_buffer,
            0,
            bytemuck::cast_slice(&vertices),
        );

        let texture_bind_group =
            self.renderer
                .device
                .create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("sprite_texture_bind_group"),
                    layout: &self.renderer.texture_bind_group_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(&texture.view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::Sampler(&texture.sampler),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::TextureView(&palette.view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 3,
                            resource: wgpu::BindingResource::Sampler(&palette.sampler),
                        },
                    ],
                });

        {
            let mut pass = self.encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("sprite_pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            pass.set_pipeline(&self.renderer.pipeline);
            pass.set_bind_group(0, &self.renderer.uniform_bind_group, &[]);
            pass.set_bind_group(1, &texture_bind_group, &[]);
            pass.set_vertex_buffer(0, self.renderer.vertex_buffer.slice(..));
            pass.set_index_buffer(
                self.renderer.index_buffer.slice(..),
                wgpu::IndexFormat::Uint16,
            );
            pass.draw_indexed(0..6, 0, 0..1);
        }
    }

    /// Issue a render pass that clears the framebuffer to the given color.
    pub fn clear(&mut self, r: f64, g: f64, b: f64) {
        let _pass = self.encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("clear_pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &self.view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color { r, g, b, a: 1.0 }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
    }

    /// Submit the recorded command buffer and present the frame.
    pub fn finish(self) {
        self.renderer
            .queue
            .submit(std::iter::once(self.encoder.finish()));
        self.output.present();
    }
}
