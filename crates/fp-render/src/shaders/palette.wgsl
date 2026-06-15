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

// MUGEN PalFX / AfterImage color tint (audit #33; full modulation set T008).
// `add.xyz` is a signed per-channel add (the caller has already folded the
// current tick's `sinadd` sine contribution into it), `mul.xyz` a per-channel
// multiply, `color.x` the grayscale-retention fraction (1.0 = full color, 0.0 =
// luminance), and `color.y` the `invertall` flag (1.0 = invert each channel,
// 0.0 = leave). The identity value {add=0, mul=1, color=(1,0,..)} leaves the
// looked-up color exactly unchanged.
struct PalFx {
    add: vec4<f32>,
    mul: vec4<f32>,
    color: vec4<f32>,
}

@group(1) @binding(4) var<uniform> palfx: PalFx;

// Rec. 601 luma weights — must match `LUMA_WEIGHTS` in params.rs so the CPU
// reference (`apply_palfx`) and this shader agree pixel-for-pixel.
const LUMA_WEIGHTS = vec3<f32>(0.299, 0.587, 0.114);

// Applies the PalFX tint to a linear RGB color: grayscale blend (color) →
// optional channel inversion (invertall) → multiply (mul) → signed add (add),
// clamped to 0..1. Mirrors `apply_palfx` in params.rs.
fn apply_palfx(rgb: vec3<f32>) -> vec3<f32> {
    let luma = dot(rgb, LUMA_WEIGHTS);
    let blended = mix(vec3<f32>(luma), rgb, palfx.color.x);
    // invertall (color.y == 1.0): flip each channel to (1 - c). mix selects the
    // inverted color when the flag is 1, the blended color when 0.
    let inverted = mix(blended, vec3<f32>(1.0) - blended, palfx.color.y);
    return clamp(inverted * palfx.mul.xyz + palfx.add.xyz, vec3<f32>(0.0), vec3<f32>(1.0));
}

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
    let index_sample = textureSample(sprite_texture, sprite_sampler, in.uv);
    let palette_index = index_sample.r;

    // Index 0 is transparent in MUGEN
    if (palette_index < 0.002) {
        discard;
    }

    let color = textureSample(palette_texture, palette_sampler, vec2<f32>(palette_index, 0.5));
    // Apply the PalFX color tint (identity = unchanged), then alpha.
    let tinted = apply_palfx(color.rgb);
    return vec4<f32>(tinted, color.a * in.alpha);
}
