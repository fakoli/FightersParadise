// Full-color RGBA image shader (stage backgrounds, full-screen art).
//
// Unlike the palette shader, this samples an `Rgba8UnormSrgb` texture directly —
// no palette lookup, no index-0 transparency, no PalFX tint. It shares the
// projection uniform (group 0) and the `SpriteVertex` layout with the sprite
// pipeline, so it reuses the same bump-allocated vertex buffer; only the bind
// group (a plain texture + sampler) and this shader differ.

struct Uniforms {
    projection: mat4x4<f32>,
}

@group(0) @binding(0) var<uniform> uniforms: Uniforms;

@group(1) @binding(0) var image_texture: texture_2d<f32>;
@group(1) @binding(1) var image_sampler: sampler;

struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) alpha: f32,
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) alpha: f32,
}

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.clip_position = uniforms.projection * vec4<f32>(in.position, 0.0, 1.0);
    out.uv = in.uv;
    out.alpha = in.alpha;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let c = textureSample(image_texture, image_sampler, in.uv);
    return vec4<f32>(c.rgb, c.a * in.alpha);
}
