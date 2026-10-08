// One instanced quad per background rect, glyph, emoji, or the background image.
// Output is straight alpha; the blend state accumulates it into the framebuffer so the
// compositor can blend the terminal over the desktop (transparency).

struct Globals {
    screen: vec2<f32>,
    _pad: vec2<f32>,
};

@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var mask_tex: texture_2d<f32>;   // R8 glyph coverage
@group(0) @binding(2) var nearest: sampler;
@group(0) @binding(3) var color_tex: texture_2d<f32>;  // RGBA color glyphs (emoji)
@group(0) @binding(4) var image_tex: texture_2d<f32>;  // background image
@group(0) @binding(5) var linear: sampler;

struct Instance {
    @location(0) pos: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) uv_pos: vec2<f32>,
    @location(3) uv_size: vec2<f32>,
    @location(4) color: vec4<f32>,
    @location(5) kind: u32,
};

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) @interpolate(flat) kind: u32,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32, inst: Instance) -> VsOut {
    let corner = vec2<f32>(f32(vi & 1u), f32((vi >> 1u) & 1u));
    let px = inst.pos + corner * inst.size;
    var out: VsOut;
    out.clip = vec4<f32>(px.x / globals.screen.x * 2.0 - 1.0, 1.0 - px.y / globals.screen.y * 2.0, 0.0, 1.0);
    out.uv = inst.uv_pos + corner * inst.uv_size; // already normalized 0..1
    out.color = inst.color;
    out.kind = inst.kind;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // Sample unconditionally: WGSL requires uniform control flow for textureSample.
    let coverage = textureSample(mask_tex, nearest, in.uv).r;
    let emoji = textureSample(color_tex, linear, in.uv);
    let image = textureSample(image_tex, linear, in.uv);
    var out = in.color;                                        // 0: solid rect
    if (in.kind == 1u) { out = vec4<f32>(in.color.rgb, in.color.a * coverage); }
    if (in.kind == 2u) { out = vec4<f32>(emoji.rgb, emoji.a * in.color.a); }
    if (in.kind == 3u) { out = vec4<f32>(image.rgb, image.a * in.color.a); }
    return out;
}
