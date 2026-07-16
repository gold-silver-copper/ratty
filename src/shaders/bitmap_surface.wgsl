// Renders one bitmap placement with independent crop, fit, filtering, and opacity.
#import bevy_sprite::mesh2d_vertex_output::VertexOutput

struct BitmapSurfaceUniform {
    uv_min: vec2<f32>,
    uv_max: vec2<f32>,
    opacity: f32,
    filter_mode: u32,
    content_min: vec2<f32>,
    content_max: vec2<f32>,
};

@group(#{MATERIAL_BIND_GROUP}) @binding(0) var bitmap_image: texture_2d<f32>;
@group(#{MATERIAL_BIND_GROUP}) @binding(1) var<uniform> params: BitmapSurfaceUniform;

fn clamped_texel(position: vec2<i32>, minimum: vec2<i32>, maximum: vec2<i32>) -> vec4<f32> {
    return textureLoad(bitmap_image, clamp(position, minimum, maximum), 0);
}

fn sample_nearest(uv: vec2<f32>, minimum: vec2<i32>, maximum: vec2<i32>) -> vec4<f32> {
    let size = vec2<f32>(textureDimensions(bitmap_image));
    let texel = vec2<i32>(floor(uv * size));
    return clamped_texel(texel, minimum, maximum);
}

fn sample_linear(uv: vec2<f32>, minimum: vec2<i32>, maximum: vec2<i32>) -> vec4<f32> {
    let size = vec2<f32>(textureDimensions(bitmap_image));
    let position = uv * size - vec2<f32>(0.5);
    let base = vec2<i32>(floor(position));
    let weight = fract(position);
    let top_left = clamped_texel(base, minimum, maximum);
    let top_right = clamped_texel(base + vec2<i32>(1, 0), minimum, maximum);
    let bottom_left = clamped_texel(base + vec2<i32>(0, 1), minimum, maximum);
    let bottom_right = clamped_texel(base + vec2<i32>(1, 1), minimum, maximum);
    return mix(mix(top_left, top_right, weight.x), mix(bottom_left, bottom_right, weight.x), weight.y);
}

@fragment
fn fragment(mesh: VertexOutput) -> @location(0) vec4<f32> {
    if (any(mesh.uv < params.content_min) || any(mesh.uv > params.content_max)) {
        return vec4<f32>(0.0);
    }

    let content_size = params.content_max - params.content_min;
    let content_uv = clamp((mesh.uv - params.content_min) / content_size, vec2<f32>(0.0), vec2<f32>(1.0));
    let source_uv = mix(params.uv_min, params.uv_max, content_uv);
    let image_size = vec2<f32>(textureDimensions(bitmap_image));
    let minimum = vec2<i32>(floor(params.uv_min * image_size));
    let maximum = vec2<i32>(ceil(params.uv_max * image_size)) - vec2<i32>(1);

    var color: vec4<f32>;
    if (params.filter_mode == 0u) {
        color = sample_nearest(source_uv, minimum, maximum);
    } else {
        color = sample_linear(source_uv, minimum, maximum);
    }
    color.a *= params.opacity;
    return color;
}
