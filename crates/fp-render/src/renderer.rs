use fp_core::{FpError, FpResult};
use wgpu::util::DeviceExt;

use crate::params::{BlendMode, SpriteDrawParams};
use crate::texture::{PaletteTexture, SpriteTexture};
use crate::vertex::{DebugVertex, SpriteVertex};

/// Quad index data: two triangles forming a rectangle.
const QUAD_INDICES: [u16; 6] = [0, 1, 2, 0, 2, 3];

/// Maximum number of debug boxes drawn per frame. Each box contributes a
/// translucent fill plus a four-edge outline; the per-frame debug vertex buffer
/// is sized for this cap and additional boxes in a single frame are dropped
/// (with a one-line debug log) rather than reallocating mid-frame.
const MAX_DEBUG_BOXES: usize = 256;

/// Triangles per debug box: one quad for the fill (2) plus one quad per edge of
/// the outline (4 edges × 2 = 8), so 10 triangles = 30 vertices.
const DEBUG_VERTS_PER_BOX: usize = 30;

/// A screen-space axis-aligned debug rectangle to outline (and lightly fill).
///
/// Coordinates are in the same top-left-origin, Y-down pixel space the sprite
/// renderer uses, so a box computed from a sprite's screen position lines up
/// exactly with the drawn sprite. Used by the collision-box debug overlay
/// (MUGEN convention: Clsn1 attack boxes drawn red, Clsn2 hurtboxes blue).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DebugBox {
    /// Left edge X in screen pixels.
    pub x: f32,
    /// Top edge Y in screen pixels.
    pub y: f32,
    /// Width in screen pixels.
    pub w: f32,
    /// Height in screen pixels.
    pub h: f32,
    /// Outline color, RGBA in 0.0–1.0. The fill reuses this color at reduced alpha.
    pub color: [f32; 4],
}

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
    pipeline_normal: wgpu::RenderPipeline,
    pipeline_additive: wgpu::RenderPipeline,
    pipeline_subtractive: wgpu::RenderPipeline,
    pipeline_debug: wgpu::RenderPipeline,
    _uniform_bind_group_layout: wgpu::BindGroupLayout,
    texture_bind_group_layout: wgpu::BindGroupLayout,
    uniform_buffer: wgpu::Buffer,
    uniform_bind_group: wgpu::BindGroup,
    vertex_buffer: wgpu::Buffer,
    index_buffer: wgpu::Buffer,
    debug_vertex_buffer: wgpu::Buffer,
}

/// Creates a render pipeline with the given blend state.
fn create_pipeline(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    shader: &wgpu::ShaderModule,
    format: wgpu::TextureFormat,
    blend_state: wgpu::BlendState,
    label: &str,
) -> wgpu::RenderPipeline {
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: shader,
            entry_point: Some("vs_main"),
            compilation_options: Default::default(),
            buffers: &[SpriteVertex::desc()],
        },
        fragment: Some(wgpu::FragmentState {
            module: shader,
            entry_point: Some("fs_main"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: Some(blend_state),
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
    })
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

        // --- Pipelines ---

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("sprite_pipeline_layout"),
            bind_group_layouts: &[&uniform_bind_group_layout, &texture_bind_group_layout],
            push_constant_ranges: &[],
        });

        let pipeline_normal = create_pipeline(
            &device,
            &pipeline_layout,
            &shader,
            surface_format,
            wgpu::BlendState::ALPHA_BLENDING,
            "sprite_pipeline_normal",
        );

        let pipeline_additive = create_pipeline(
            &device,
            &pipeline_layout,
            &shader,
            surface_format,
            wgpu::BlendState {
                color: wgpu::BlendComponent {
                    src_factor: wgpu::BlendFactor::SrcAlpha,
                    dst_factor: wgpu::BlendFactor::One,
                    operation: wgpu::BlendOperation::Add,
                },
                alpha: wgpu::BlendComponent {
                    src_factor: wgpu::BlendFactor::One,
                    dst_factor: wgpu::BlendFactor::One,
                    operation: wgpu::BlendOperation::Add,
                },
            },
            "sprite_pipeline_additive",
        );

        let pipeline_subtractive = create_pipeline(
            &device,
            &pipeline_layout,
            &shader,
            surface_format,
            wgpu::BlendState {
                color: wgpu::BlendComponent {
                    src_factor: wgpu::BlendFactor::SrcAlpha,
                    dst_factor: wgpu::BlendFactor::One,
                    operation: wgpu::BlendOperation::ReverseSubtract,
                },
                alpha: wgpu::BlendComponent::OVER,
            },
            "sprite_pipeline_subtractive",
        );

        // --- Debug overlay pipeline (unlit colored triangles) ---
        // Shares the projection uniform (group 0) but uses no textures, so it
        // gets its own pipeline layout and a separate WGSL shader.
        let debug_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("debug_shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shaders/debug.wgsl").into()),
        });
        let debug_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("debug_pipeline_layout"),
                bind_group_layouts: &[&uniform_bind_group_layout],
                push_constant_ranges: &[],
            });
        let pipeline_debug =
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("debug_pipeline"),
                layout: Some(&debug_pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &debug_shader,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[DebugVertex::desc()],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &debug_shader,
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

        // Per-frame debug-overlay vertex buffer, sized for the box cap. One
        // box's geometry is written and drawn per `draw_debug_box` call.
        let debug_vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("debug_vertex_buffer"),
            size: (std::mem::size_of::<DebugVertex>() * DEBUG_VERTS_PER_BOX * MAX_DEBUG_BOXES)
                as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Ok(Self {
            device,
            queue,
            surface,
            surface_config,
            pipeline_normal,
            pipeline_additive,
            pipeline_subtractive,
            pipeline_debug,
            _uniform_bind_group_layout: uniform_bind_group_layout,
            texture_bind_group_layout,
            uniform_buffer,
            uniform_bind_group,
            vertex_buffer,
            index_buffer,
            debug_vertex_buffer,
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
            debug_box_count: 0,
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
    /// Number of debug boxes already written into the shared debug vertex buffer
    /// this frame; each [`draw_debug_box`](Self::draw_debug_box) call uses the
    /// next slot so earlier boxes are not overwritten.
    debug_box_count: usize,
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

        // Corner positions relative to sprite origin, before rotation.
        let mut corners = [
            [params.x,     params.y    ],
            [params.x + w, params.y    ],
            [params.x + w, params.y + h],
            [params.x,     params.y + h],
        ];

        // Rotate around sprite center if angle is non-zero.
        if params.angle != 0.0 {
            let cx = params.x + w * 0.5;
            let cy = params.y + h * 0.5;
            let cos = params.angle.cos();
            let sin = params.angle.sin();
            for corner in &mut corners {
                let dx = corner[0] - cx;
                let dy = corner[1] - cy;
                corner[0] = cx + dx * cos - dy * sin;
                corner[1] = cy + dx * sin + dy * cos;
            }
        }

        let a = params.alpha;
        let vertices = [
            SpriteVertex { position: corners[0], uv: [u_left,  v_top   ], alpha: a },
            SpriteVertex { position: corners[1], uv: [u_right, v_top   ], alpha: a },
            SpriteVertex { position: corners[2], uv: [u_right, v_bottom], alpha: a },
            SpriteVertex { position: corners[3], uv: [u_left,  v_bottom], alpha: a },
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

            let pipeline = match params.blend {
                BlendMode::Normal => &self.renderer.pipeline_normal,
                BlendMode::Additive => &self.renderer.pipeline_additive,
                BlendMode::Subtractive => &self.renderer.pipeline_subtractive,
            };
            pass.set_pipeline(pipeline);
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

    /// Draw a single axis-aligned debug box: a translucent fill plus a solid
    /// outline, in screen-space pixels, over whatever is already drawn.
    ///
    /// Intended for the collision-box overlay (Clsn1 red, Clsn2 blue). The
    /// outline thickness is a fixed pixel width; a box thinner than twice that
    /// degrades to a (near-)solid fill, which is fine for a debug aid. Up to
    /// [`MAX_DEBUG_BOXES`] boxes are accepted per frame; further calls are
    /// dropped with a debug log rather than overflowing the buffer.
    pub fn draw_debug_box(&mut self, b: &DebugBox) {
        /// Outline edge thickness in screen pixels.
        const OUTLINE: f32 = 1.5;
        /// Fill alpha as a fraction of the outline color's alpha.
        const FILL_ALPHA_SCALE: f32 = 0.18;

        if self.debug_box_count >= MAX_DEBUG_BOXES {
            tracing::debug!(
                "debug overlay: more than {MAX_DEBUG_BOXES} boxes in one frame; dropping extras"
            );
            return;
        }

        let verts = build_debug_box_verts(b, OUTLINE, FILL_ALPHA_SCALE);
        let slot = self.debug_box_count;
        let offset = (slot * DEBUG_VERTS_PER_BOX * std::mem::size_of::<DebugVertex>()) as u64;
        self.renderer.queue.write_buffer(
            &self.renderer.debug_vertex_buffer,
            offset,
            bytemuck::cast_slice(&verts),
        );

        {
            let mut pass = self.encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("debug_box_pass"),
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
            pass.set_pipeline(&self.renderer.pipeline_debug);
            pass.set_bind_group(0, &self.renderer.uniform_bind_group, &[]);
            let end = offset + (DEBUG_VERTS_PER_BOX * std::mem::size_of::<DebugVertex>()) as u64;
            pass.set_vertex_buffer(0, self.renderer.debug_vertex_buffer.slice(offset..end));
            pass.draw(0..DEBUG_VERTS_PER_BOX as u32, 0..1);
        }

        self.debug_box_count += 1;
    }

    /// Submit the recorded command buffer and present the frame.
    pub fn finish(self) {
        self.renderer
            .queue
            .submit(std::iter::once(self.encoder.finish()));
        self.output.present();
    }
}

/// Builds the [`DEBUG_VERTS_PER_BOX`] triangle-list vertices for one debug box:
/// a translucent fill quad followed by four thin outline-edge quads (top,
/// bottom, left, right).
///
/// `outline` is the edge thickness in pixels; `fill_alpha_scale` scales the
/// outline color's alpha for the fill. The box is normalized so a negative
/// width/height still produces a sane rect. Pure (no GPU state); unit-tested.
fn build_debug_box_verts(
    b: &DebugBox,
    outline: f32,
    fill_alpha_scale: f32,
) -> [DebugVertex; DEBUG_VERTS_PER_BOX] {
    // Normalize to a top-left origin with non-negative extents.
    let x0 = b.x.min(b.x + b.w);
    let y0 = b.y.min(b.y + b.h);
    let w = b.w.abs();
    let h = b.h.abs();
    let x1 = x0 + w;
    let y1 = y0 + h;

    // Clamp the outline so a thin box does not produce inverted edge rects.
    let t = outline.min(w * 0.5).min(h * 0.5).max(0.0);

    let fill = [
        b.color[0],
        b.color[1],
        b.color[2],
        b.color[3] * fill_alpha_scale,
    ];
    let line = b.color;

    let mut verts = [DebugVertex {
        position: [0.0, 0.0],
        color: fill,
    }; DEBUG_VERTS_PER_BOX];
    let mut i = 0;
    // Pushes the two triangles (6 vertices) of an axis-aligned rect into `verts`.
    let mut push_rect = |rx0: f32, ry0: f32, rx1: f32, ry1: f32, color: [f32; 4]| {
        let quad = [
            [rx0, ry0],
            [rx1, ry0],
            [rx1, ry1],
            [rx0, ry0],
            [rx1, ry1],
            [rx0, ry1],
        ];
        for p in quad {
            verts[i] = DebugVertex {
                position: p,
                color,
            };
            i += 1;
        }
    };

    // Fill (whole box).
    push_rect(x0, y0, x1, y1, fill);
    // Outline: four edge strips of thickness `t`.
    push_rect(x0, y0, x1, y0 + t, line); // top
    push_rect(x0, y1 - t, x1, y1, line); // bottom
    push_rect(x0, y0 + t, x0 + t, y1 - t, line); // left (between top/bottom strips)
    push_rect(x1 - t, y0 + t, x1, y1 - t, line); // right

    verts
}

/// Builds the four quad vertices for a sprite draw, used internally and for testing.
///
/// Returns `[top-left, top-right, bottom-right, bottom-left]` vertices.
#[cfg(test)]
pub(crate) fn build_sprite_quad(
    width: u32,
    height: u32,
    params: &SpriteDrawParams,
) -> [SpriteVertex; 4] {
    let w = width as f32 * params.scale_x;
    let h = height as f32 * params.scale_y;

    let (u_left, u_right) = if params.flip_h { (1.0, 0.0) } else { (0.0, 1.0) };
    let (v_top, v_bottom) = if params.flip_v { (1.0, 0.0) } else { (0.0, 1.0) };

    let mut corners = [
        [params.x,     params.y    ],
        [params.x + w, params.y    ],
        [params.x + w, params.y + h],
        [params.x,     params.y + h],
    ];

    if params.angle != 0.0 {
        let cx = params.x + w * 0.5;
        let cy = params.y + h * 0.5;
        let cos = params.angle.cos();
        let sin = params.angle.sin();
        for corner in &mut corners {
            let dx = corner[0] - cx;
            let dy = corner[1] - cy;
            corner[0] = cx + dx * cos - dy * sin;
            corner[1] = cy + dx * sin + dy * cos;
        }
    }

    let a = params.alpha;
    [
        SpriteVertex { position: corners[0], uv: [u_left,  v_top   ], alpha: a },
        SpriteVertex { position: corners[1], uv: [u_right, v_top   ], alpha: a },
        SpriteVertex { position: corners[2], uv: [u_right, v_bottom], alpha: a },
        SpriteVertex { position: corners[3], uv: [u_left,  v_bottom], alpha: a },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::FRAC_PI_2;

    fn approx_eq(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    fn positions_approx_eq(a: [f32; 2], b: [f32; 2]) -> bool {
        approx_eq(a[0], b[0]) && approx_eq(a[1], b[1])
    }

    #[test]
    fn default_params_produces_identity_quad() {
        let params = SpriteDrawParams {
            x: 10.0,
            y: 20.0,
            ..Default::default()
        };
        let verts = build_sprite_quad(64, 32, &params);

        assert_eq!(verts[0].position, [10.0, 20.0]);
        assert_eq!(verts[1].position, [74.0, 20.0]);
        assert_eq!(verts[2].position, [74.0, 52.0]);
        assert_eq!(verts[3].position, [10.0, 52.0]);

        // UVs: normal orientation
        assert_eq!(verts[0].uv, [0.0, 0.0]);
        assert_eq!(verts[1].uv, [1.0, 0.0]);
        assert_eq!(verts[2].uv, [1.0, 1.0]);
        assert_eq!(verts[3].uv, [0.0, 1.0]);

        // Alpha defaults to 1.0
        for v in &verts {
            assert_eq!(v.alpha, 1.0);
        }
    }

    #[test]
    fn flip_h_swaps_u_coordinates() {
        let params = SpriteDrawParams {
            flip_h: true,
            ..Default::default()
        };
        let verts = build_sprite_quad(64, 32, &params);

        assert_eq!(verts[0].uv[0], 1.0); // left vertex gets u=1
        assert_eq!(verts[1].uv[0], 0.0); // right vertex gets u=0
    }

    #[test]
    fn flip_v_swaps_v_coordinates() {
        let params = SpriteDrawParams {
            flip_v: true,
            ..Default::default()
        };
        let verts = build_sprite_quad(64, 32, &params);

        assert_eq!(verts[0].uv[1], 1.0); // top vertex gets v=1
        assert_eq!(verts[3].uv[1], 0.0); // bottom vertex gets v=0
    }

    #[test]
    fn scale_affects_quad_size() {
        let params = SpriteDrawParams {
            scale_x: 2.0,
            scale_y: 0.5,
            ..Default::default()
        };
        let verts = build_sprite_quad(64, 32, &params);

        // Width should be 64*2 = 128, height should be 32*0.5 = 16
        assert_eq!(verts[1].position[0], 128.0);
        assert_eq!(verts[2].position[1], 16.0);
    }

    #[test]
    fn alpha_propagates_to_vertices() {
        let params = SpriteDrawParams {
            alpha: 0.5,
            ..Default::default()
        };
        let verts = build_sprite_quad(64, 32, &params);

        for v in &verts {
            assert_eq!(v.alpha, 0.5);
        }
    }

    #[test]
    fn rotation_90_degrees_rotates_around_center() {
        // 64x64 sprite at origin, rotated 90 degrees CW
        let params = SpriteDrawParams {
            x: 0.0,
            y: 0.0,
            angle: FRAC_PI_2,
            ..Default::default()
        };
        let verts = build_sprite_quad(64, 64, &params);

        // Center is (32, 32). After 90° rotation:
        // TL (0,0) -> (64, 0)
        // TR (64,0) -> (64, 64)
        // BR (64,64) -> (0, 64)
        // BL (0,64) -> (0, 0)
        assert!(positions_approx_eq(verts[0].position, [64.0, 0.0]),
            "TL got {:?}", verts[0].position);
        assert!(positions_approx_eq(verts[1].position, [64.0, 64.0]),
            "TR got {:?}", verts[1].position);
        assert!(positions_approx_eq(verts[2].position, [0.0, 64.0]),
            "BR got {:?}", verts[2].position);
        assert!(positions_approx_eq(verts[3].position, [0.0, 0.0]),
            "BL got {:?}", verts[3].position);
    }

    #[test]
    fn zero_angle_no_rotation() {
        let params = SpriteDrawParams {
            x: 10.0,
            y: 20.0,
            angle: 0.0,
            ..Default::default()
        };
        let verts = build_sprite_quad(64, 32, &params);

        // Should be identical to no-rotation case
        assert_eq!(verts[0].position, [10.0, 20.0]);
        assert_eq!(verts[1].position, [74.0, 20.0]);
    }

    #[test]
    fn blend_mode_default_is_normal() {
        let params = SpriteDrawParams::default();
        assert_eq!(params.blend, BlendMode::Normal);
    }

    #[test]
    fn debug_box_vertex_count_is_fixed() {
        let b = DebugBox {
            x: 10.0,
            y: 20.0,
            w: 40.0,
            h: 30.0,
            color: [1.0, 0.0, 0.0, 1.0],
        };
        let verts = build_debug_box_verts(&b, 1.5, 0.18);
        assert_eq!(verts.len(), DEBUG_VERTS_PER_BOX);
    }

    #[test]
    fn debug_box_fill_covers_full_rect_extents() {
        let b = DebugBox {
            x: 10.0,
            y: 20.0,
            w: 40.0,
            h: 30.0,
            color: [0.0, 0.0, 1.0, 1.0],
        };
        let verts = build_debug_box_verts(&b, 1.5, 0.18);
        // The first 6 vertices are the fill quad spanning the whole box.
        let xs: Vec<f32> = verts[0..6].iter().map(|v| v.position[0]).collect();
        let ys: Vec<f32> = verts[0..6].iter().map(|v| v.position[1]).collect();
        let min_x = xs.iter().cloned().fold(f32::INFINITY, f32::min);
        let max_x = xs.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        let min_y = ys.iter().cloned().fold(f32::INFINITY, f32::min);
        let max_y = ys.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        assert!(approx_eq(min_x, 10.0));
        assert!(approx_eq(max_x, 50.0));
        assert!(approx_eq(min_y, 20.0));
        assert!(approx_eq(max_y, 50.0));
    }

    #[test]
    fn debug_box_fill_alpha_is_scaled_outline_is_solid() {
        let b = DebugBox {
            x: 0.0,
            y: 0.0,
            w: 40.0,
            h: 40.0,
            color: [1.0, 0.0, 0.0, 1.0],
        };
        let verts = build_debug_box_verts(&b, 2.0, 0.25);
        // Fill quad (first 6 verts) uses the scaled alpha.
        assert!(approx_eq(verts[0].color[3], 0.25));
        // The remaining outline-edge verts keep the full alpha.
        assert!(approx_eq(verts[6].color[3], 1.0));
    }

    #[test]
    fn debug_box_normalizes_negative_extents() {
        // A box authored with negative width/height should normalize to the same
        // extents as its positive twin.
        let neg = DebugBox {
            x: 50.0,
            y: 50.0,
            w: -40.0,
            h: -30.0,
            color: [0.0, 1.0, 0.0, 1.0],
        };
        let verts = build_debug_box_verts(&neg, 1.5, 0.18);
        let xs: Vec<f32> = verts[0..6].iter().map(|v| v.position[0]).collect();
        let min_x = xs.iter().cloned().fold(f32::INFINITY, f32::min);
        let max_x = xs.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        assert!(approx_eq(min_x, 10.0));
        assert!(approx_eq(max_x, 50.0));
    }
}
