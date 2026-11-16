// Instanced quad pipelines in pixel space: solid rects, coverage glyphs
// tinted by the text color, and premultiplied color glyphs.

struct Globals {
    viewport: vec2<f32>,
    _pad: vec2<f32>,
};

@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var atlas: texture_2d<f32>;
@group(0) @binding(2) var atlas_sampler: sampler;
@group(0) @binding(3) var color_atlas: texture_2d<f32>;
// One Kitty graphics picture per draw, straight alpha, scaled smoothly.
@group(1) @binding(0) var picture: texture_2d<f32>;
@group(1) @binding(1) var picture_sampler: sampler;

struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) uv: vec2<f32>,
};

// Corner of a 4-vertex triangle strip: (0,0) (1,0) (0,1) (1,1).
fn corner(index: u32) -> vec2<f32> {
    return vec2<f32>(f32(index & 1u), f32((index >> 1u) & 1u));
}

fn to_clip(pixel: vec2<f32>) -> vec4<f32> {
    let ndc = pixel / globals.viewport * 2.0 - 1.0;
    return vec4<f32>(ndc.x, -ndc.y, 0.0, 1.0);
}

@vertex
fn rect_vs(
    @builtin(vertex_index) index: u32,
    @location(0) pos: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) color: vec4<f32>,
) -> VertexOut {
    var out: VertexOut;
    out.position = to_clip(pos + corner(index) * size);
    out.color = color;
    out.uv = vec2<f32>(0.0, 0.0);
    return out;
}

@fragment
fn rect_fs(in: VertexOut) -> @location(0) vec4<f32> {
    return in.color;
}

@vertex
fn glyph_vs(
    @builtin(vertex_index) index: u32,
    @location(0) pos: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) uv_pos: vec2<f32>,
    @location(3) uv_size: vec2<f32>,
    @location(4) color: vec4<f32>,
) -> VertexOut {
    let c = corner(index);
    var out: VertexOut;
    out.position = to_clip(pos + c * size);
    out.color = color;
    out.uv = uv_pos + c * uv_size;
    return out;
}

@fragment
fn glyph_fs(in: VertexOut) -> @location(0) vec4<f32> {
    let coverage = textureSample(atlas, atlas_sampler, in.uv).r;
    return vec4<f32>(in.color.rgb, in.color.a * coverage);
}

// Premultiplied; the instance alpha fades faint text.
@fragment
fn color_glyph_fs(in: VertexOut) -> @location(0) vec4<f32> {
    return textureSample(color_atlas, atlas_sampler, in.uv) * in.color.a;
}

@fragment
fn picture_fs(in: VertexOut) -> @location(0) vec4<f32> {
    return textureSample(picture, picture_sampler, in.uv);
}
