// One instanced quad per background rect or glyph. Output is straight alpha; the
// blend state accumulates it into the framebuffer so the compositor can blend the
// terminal over the desktop (transparency).

struct Globals {
    screen: vec2<f32>,
    atlas: vec2<f32>,
};

@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var atlas_tex: texture_2d<f32>;
@group(0) @binding(2) var atlas_smp: sampler;

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
    out.uv = (inst.uv_pos + corner * inst.uv_size) / globals.atlas;
    out.color = inst.color;
    out.kind = inst.kind;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // Sample unconditionally: WGSL requires uniform control flow for textureSample.
    let coverage = textureSample(atlas_tex, atlas_smp, in.uv).r;
    let a = select(in.color.a, in.color.a * coverage, in.kind == 1u);
    return vec4<f32>(in.color.rgb, a);
}
