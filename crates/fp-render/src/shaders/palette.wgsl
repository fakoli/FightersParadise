// Palette-indexed sprite shader.
//
// MUGEN sprites are 256-color indexed images. This shader samples an R8Unorm
// sprite texture (palette indices 0–255 mapped to 0.0–1.0) and performs a
// palette lookup in a 256×1 RGBA texture. Index 0 is treated as transparent.

struct Uniforms {
    projection: mat4x4<f32>,
}

@group(0) @binding(0) var<uniform> uniforms: Uniforms;

@group(1) @binding(0) var sprite_texture: texture_2d<f32>;
@group(1) @binding(1) var sprite_sampler: sampler;
@group(1) @binding(2) var palette_texture: texture_2d<f32>;
@group(1) @binding(3) var palette_sampler: sampler;

struct VertexInput {
    @location(0) position: vec2<f32>,
    @location(1) uv: vec2<f32>,
}

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs_main(in: VertexInput) -> VertexOutput {
    var out: VertexOutput;
    out.clip_position = uniforms.projection * vec4<f32>(in.position, 0.0, 1.0);
    out.uv = in.uv;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    let index_sample = textureSample(sprite_texture, sprite_sampler, in.uv);
    let palette_index = index_sample.r;

    // Index 0 is transparent in MUGEN
    if (palette_index < 0.002) {
        discard;
    }

    let color = textureSample(palette_texture, palette_sampler, vec2<f32>(palette_index, 0.5));
    return color;
}
