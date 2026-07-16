//! Rendering material and layout math for bitmap-surface placements.

use bevy::asset::uuid_handle;
use bevy::math::{UVec2, Vec2};
use bevy::prelude::{Asset, Handle, Image, TypePath};
use bevy::render::render_resource::{AsBindGroup, ShaderType};
use bevy::shader::{Shader, ShaderRef};
use bevy::sprite_render::{AlphaMode2d, Material2d};

use crate::bitmap::{BitmapFit, SourceRect};

/// Handle for the embedded bitmap-surface shader.
pub(crate) const BITMAP_SURFACE_SHADER: Handle<Shader> =
    uuid_handle!("226f825e-49b6-4cae-bfc4-a03efadfb255");

/// A material for one bitmap placement.
///
/// Placements keep independent parameters while sharing the same `Image`
/// handle, including when their filtering modes differ.
#[derive(Asset, TypePath, AsBindGroup, Clone)]
pub struct BitmapSurfaceMaterial {
    /// Shared bitmap image.
    #[texture(0)]
    pub image: Handle<Image>,
    /// Crop, fit, filtering, and opacity parameters for this placement.
    #[uniform(1)]
    pub params: BitmapSurfaceUniform,
}

/// Shader parameters resolved for one bitmap placement.
#[derive(Clone, Copy, Debug, PartialEq, ShaderType)]
pub struct BitmapSurfaceUniform {
    /// Top-left normalized source coordinate.
    pub uv_min: Vec2,
    /// Bottom-right normalized source coordinate.
    pub uv_max: Vec2,
    /// Placement opacity in the inclusive range `[0, 1]`.
    pub opacity: f32,
    /// `0` for nearest-neighbor filtering or `1` for linear filtering.
    pub filter_mode: u32,
    /// Top-left normalized destination content bound.
    pub content_min: Vec2,
    /// Bottom-right normalized destination content bound.
    pub content_max: Vec2,
}

impl Material2d for BitmapSurfaceMaterial {
    fn fragment_shader() -> ShaderRef {
        BITMAP_SURFACE_SHADER.into()
    }

    fn alpha_mode(&self) -> AlphaMode2d {
        AlphaMode2d::Blend
    }
}

/// Resolved normalized crop and destination-content bounds.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ResolvedBitmapLayout {
    /// Top-left normalized source coordinate.
    pub uv_min: Vec2,
    /// Bottom-right normalized source coordinate.
    pub uv_max: Vec2,
    /// Top-left normalized destination content bound.
    pub content_min: Vec2,
    /// Bottom-right normalized destination content bound.
    pub content_max: Vec2,
}

/// Resolves source cropping and fit into normalized shader coordinates.
///
/// Source rectangles are clamped to the bitmap bounds. Protocol state rejects
/// empty sources and destinations before calling this function; zero-sized
/// input defensively resolves to an empty layout.
pub fn resolve_bitmap_layout(
    bitmap_size: UVec2,
    source: Option<SourceRect>,
    destination_pixels: Vec2,
    fit: BitmapFit,
) -> ResolvedBitmapLayout {
    if bitmap_size.x == 0
        || bitmap_size.y == 0
        || destination_pixels.x <= 0.0
        || destination_pixels.y <= 0.0
    {
        return ResolvedBitmapLayout::default();
    }

    let (source_min, source_max) = clamped_source(bitmap_size, source);
    let source_size = source_max - source_min;
    if source_size.x <= 0.0 || source_size.y <= 0.0 {
        return ResolvedBitmapLayout::default();
    }

    let bitmap_size = bitmap_size.as_vec2();
    let mut layout = ResolvedBitmapLayout {
        uv_min: source_min / bitmap_size,
        uv_max: source_max / bitmap_size,
        content_min: Vec2::ZERO,
        content_max: Vec2::ONE,
    };

    match fit {
        BitmapFit::Fill => {}
        BitmapFit::Contain => {
            let scale =
                (destination_pixels.x / source_size.x).min(destination_pixels.y / source_size.y);
            let content_size = source_size * scale / destination_pixels;
            layout.content_min = (Vec2::ONE - content_size) * 0.5;
            layout.content_max = layout.content_min + content_size;
        }
        BitmapFit::Cover => {
            let scale =
                (destination_pixels.x / source_size.x).max(destination_pixels.y / source_size.y);
            let visible_source_size = destination_pixels / scale;
            let crop = (source_size - visible_source_size) * 0.5;
            layout.uv_min = (source_min + crop) / bitmap_size;
            layout.uv_max = (source_max - crop) / bitmap_size;
        }
    }

    layout
}

fn clamped_source(bitmap_size: UVec2, source: Option<SourceRect>) -> (Vec2, Vec2) {
    let Some(source) = source else {
        return (Vec2::ZERO, bitmap_size.as_vec2());
    };

    let min_x = source.x.min(bitmap_size.x);
    let min_y = source.y.min(bitmap_size.y);
    let max_x = source.x.saturating_add(source.width).min(bitmap_size.x);
    let max_y = source.y.saturating_add(source.height).min(bitmap_size.y);
    (
        Vec2::new(min_x as f32, min_y as f32),
        Vec2::new(max_x as f32, max_y as f32),
    )
}

#[cfg(test)]
mod tests {
    use bevy::math::{UVec2, Vec2};

    use super::*;
    use crate::bitmap::{BitmapFit, SourceRect};

    fn assert_vec2(actual: Vec2, expected: Vec2) {
        assert!(
            actual.abs_diff_eq(expected, 1.0e-6),
            "expected {expected:?}, got {actual:?}"
        );
    }

    #[test]
    fn fill_maps_the_selected_source_to_the_entire_destination() {
        let layout = resolve_bitmap_layout(
            UVec2::new(400, 200),
            None,
            Vec2::new(300.0, 300.0),
            BitmapFit::Fill,
        );

        assert_vec2(layout.uv_min, Vec2::ZERO);
        assert_vec2(layout.uv_max, Vec2::ONE);
        assert_vec2(layout.content_min, Vec2::ZERO);
        assert_vec2(layout.content_max, Vec2::ONE);
    }

    #[test]
    fn contain_letterboxes_landscape_content_vertically() {
        let layout = resolve_bitmap_layout(
            UVec2::new(400, 200),
            None,
            Vec2::new(300.0, 300.0),
            BitmapFit::Contain,
        );

        assert_vec2(layout.uv_min, Vec2::ZERO);
        assert_vec2(layout.uv_max, Vec2::ONE);
        assert_vec2(layout.content_min, Vec2::new(0.0, 0.25));
        assert_vec2(layout.content_max, Vec2::new(1.0, 0.75));
    }

    #[test]
    fn contain_letterboxes_portrait_content_horizontally() {
        let layout = resolve_bitmap_layout(
            UVec2::new(200, 400),
            None,
            Vec2::new(300.0, 300.0),
            BitmapFit::Contain,
        );

        assert_vec2(layout.content_min, Vec2::new(0.25, 0.0));
        assert_vec2(layout.content_max, Vec2::new(0.75, 1.0));
    }

    #[test]
    fn cover_crops_landscape_content_symmetrically() {
        let layout = resolve_bitmap_layout(
            UVec2::new(400, 200),
            None,
            Vec2::new(300.0, 300.0),
            BitmapFit::Cover,
        );

        assert_vec2(layout.uv_min, Vec2::new(0.25, 0.0));
        assert_vec2(layout.uv_max, Vec2::new(0.75, 1.0));
        assert_vec2(layout.content_min, Vec2::ZERO);
        assert_vec2(layout.content_max, Vec2::ONE);
    }

    #[test]
    fn cover_crops_portrait_content_symmetrically() {
        let layout = resolve_bitmap_layout(
            UVec2::new(200, 400),
            None,
            Vec2::new(300.0, 300.0),
            BitmapFit::Cover,
        );

        assert_vec2(layout.uv_min, Vec2::new(0.0, 0.25));
        assert_vec2(layout.uv_max, Vec2::new(1.0, 0.75));
    }

    #[test]
    fn explicit_crop_is_normalized_against_the_full_bitmap() {
        let layout = resolve_bitmap_layout(
            UVec2::new(400, 200),
            Some(SourceRect {
                x: 100,
                y: 50,
                width: 200,
                height: 100,
            }),
            Vec2::new(200.0, 100.0),
            BitmapFit::Fill,
        );

        assert_vec2(layout.uv_min, Vec2::new(0.25, 0.25));
        assert_vec2(layout.uv_max, Vec2::new(0.75, 0.75));
    }

    #[test]
    fn source_crop_is_clamped_to_bitmap_bounds() {
        let layout = resolve_bitmap_layout(
            UVec2::new(400, 200),
            Some(SourceRect {
                x: 300,
                y: 100,
                width: 500,
                height: 500,
            }),
            Vec2::new(100.0, 100.0),
            BitmapFit::Fill,
        );

        assert_vec2(layout.uv_min, Vec2::new(0.75, 0.5));
        assert_vec2(layout.uv_max, Vec2::ONE);
    }
}
