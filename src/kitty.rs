//! Renderer-side integration for rio-vt's native Kitty graphics state.
//!
//! Ratty deliberately does not parse Kitty APCs here. Complete APC frames are
//! forwarded unchanged to rio-vt; this module only owns Bevy asset handles and
//! converts rio-vt's direct/Unicode-placeholder placement geometry into draw
//! records.

use std::collections::HashMap;

use bevy::prelude::*;
use rio_graphics::GraphicData;
use rio_vt::ansi::graphics::{OverlayViewport, UpdateQueues, kitty_overlay_geometry};
use rio_vt::ansi::kitty_virtual::{IncompletePlacement, PlaceholderRun, compute_run_geometry};
use rio_vt::crosswords::pos::Column;

use crate::bitmap_material::BitmapSurfaceMaterial;
use crate::vt::{self, VtTerminal};

/// Marker for one native Kitty draw entity.
#[derive(Component)]
pub struct TerminalKittyPlacement;

/// Stable Bevy image asset for one Kitty protocol image id.
pub(crate) struct KittyImageAsset {
    pub(crate) handle: Handle<Image>,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

/// Renderer-owned state. Pixel lifetime and placement truth remain in rio-vt.
#[derive(Default)]
pub(crate) struct KittyRenderCache {
    pub(crate) images: HashMap<u32, KittyImageAsset>,
    /// Final queued frontend action per image id. Replacements coalesce so a
    /// burst of retransmissions cannot retain one decoded pixel buffer per APC.
    pub(crate) pending_updates: HashMap<u32, Option<GraphicData>>,
    pub(crate) meshes: Vec<Handle<Mesh>>,
    pub(crate) sprite_materials: Vec<Handle<BitmapSurfaceMaterial>>,
    pub(crate) plane_materials: Vec<Handle<StandardMaterial>>,
    dirty: bool,
}

impl KittyRenderCache {
    pub(crate) fn queue_updates(&mut self, updates: impl IntoIterator<Item = UpdateQueues>) {
        let mut changed = false;
        for update in updates {
            for key in update.remove_queue {
                let Ok(image_id) = u32::try_from(key) else {
                    continue;
                };
                self.pending_updates.insert(image_id, None);
                changed = true;
            }
            for (image_id, graphic) in update.pending_images {
                self.pending_updates.insert(image_id, Some(graphic));
                changed = true;
            }
        }
        self.dirty |= changed;
    }

    pub(crate) fn is_dirty(&self) -> bool {
        self.dirty || !self.pending_updates.is_empty()
    }

    pub(crate) fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    pub(crate) fn mark_dirty(&mut self) {
        self.dirty = true;
    }
}

/// One resolved native Kitty quad in terminal-local top-left pixel space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct KittyDraw {
    pub(crate) image_id: u32,
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
    pub(crate) source_rect: [f32; 4],
    pub(crate) z_index: i32,
    /// Resolved Bevy depth after sorting by the Kitty `(z, image_id)` rule.
    pub(crate) depth: f32,
}

/// Resolves all active-screen direct and Unicode-placeholder placements.
pub(crate) fn collect_draws(
    term: &VtTerminal,
    cache: &KittyRenderCache,
    cell_width: f32,
    cell_height: f32,
    viewport_width: f32,
    viewport_height: f32,
) -> Vec<KittyDraw> {
    if cell_width <= 0.0 || cell_height <= 0.0 {
        return Vec::new();
    }

    let viewport = OverlayViewport {
        cell_width,
        cell_height,
        origin_x: 0.0,
        origin_y: 0.0,
        history_size: i64::try_from(term.lines_evicted()).unwrap_or(i64::MAX)
            + i64::try_from(term.history_size()).unwrap_or(i64::MAX),
        display_offset: i64::try_from(term.display_offset()).unwrap_or(i64::MAX),
        screen_lines: i64::try_from(term.screen_lines()).unwrap_or(i64::MAX),
    };
    let mut draws = Vec::new();

    for placement in term.graphics.kitty_placements.values() {
        let Some(image) = cache.images.get(&placement.image_id) else {
            continue;
        };
        let Some(geometry) = kitty_overlay_geometry(
            placement,
            image.width as usize,
            image.height as usize,
            &viewport,
        ) else {
            continue;
        };
        let mut draw = KittyDraw {
            image_id: placement.image_id,
            x: geometry.x,
            y: geometry.y,
            width: geometry.width,
            height: geometry.height,
            source_rect: geometry.source_rect,
            z_index: placement.z_index,
            depth: 0.0,
        };
        if clip_draw(&mut draw, viewport_width, viewport_height) {
            draws.push(draw);
        }
    }

    collect_placeholder_draws(
        term,
        cache,
        cell_width,
        cell_height,
        viewport_width,
        viewport_height,
        &mut draws,
    );
    resolve_stack_depths(&mut draws);
    draws
}

fn resolve_stack_depths(draws: &mut [KittyDraw]) {
    draws.sort_by_key(|draw| (draw.z_index, draw.image_id));
    let negative_count = draws.iter().take_while(|draw| draw.z_index < 0).count();
    for (index, draw) in draws[..negative_count].iter_mut().enumerate() {
        draw.depth = distributed_depth(-1.5, index, negative_count);
    }
    let nonnegative_count = draws.len().saturating_sub(negative_count);
    for (index, draw) in draws[negative_count..].iter_mut().enumerate() {
        draw.depth = distributed_depth(1.5, index, nonnegative_count);
    }
}

fn distributed_depth(center: f32, index: usize, count: usize) -> f32 {
    if count <= 1 {
        return center;
    }
    center - 0.4 + 0.8 * index as f32 / (count - 1) as f32
}

fn collect_placeholder_draws(
    term: &VtTerminal,
    cache: &KittyRenderCache,
    cell_width: f32,
    cell_height: f32,
    viewport_width: f32,
    viewport_height: f32,
    draws: &mut Vec<KittyDraw>,
) {
    let styles = vt::styles(term);
    let columns = term.columns();
    for screen_line in 0..term.screen_lines() {
        let Ok(row) = u16::try_from(screen_line) else {
            break;
        };
        let Some(grid_row) = vt::visible_row(term, row) else {
            continue;
        };
        if !grid_row.kitty_virtual_placeholder {
            continue;
        }

        let mut current: Option<(IncompletePlacement, usize)> = None;
        for col in 0..columns {
            let square = grid_row[Column(col)];
            if square.c() != rio_vt::ansi::graphics::KITTY_PLACEHOLDER {
                flush_placeholder_run(
                    term,
                    cache,
                    current.take(),
                    screen_line,
                    cell_width,
                    cell_height,
                    viewport_width,
                    viewport_height,
                    draws,
                );
                continue;
            }

            let style = styles
                .get(usize::from(square.style_id()))
                .copied()
                .unwrap_or_default();
            let combining = square
                .extras_id()
                .and_then(|id| term.grid.extras_table.get(id))
                .map_or(&[][..], |extras| extras.zerowidth.as_slice());
            let next = IncompletePlacement::from_cell(style.fg, style.underline_color, combining);
            match current.as_mut() {
                Some((run, _)) if run.can_append(&next) => run.append(),
                _ => {
                    flush_placeholder_run(
                        term,
                        cache,
                        current.take(),
                        screen_line,
                        cell_width,
                        cell_height,
                        viewport_width,
                        viewport_height,
                        draws,
                    );
                    current = Some((next, col));
                }
            }
        }
        flush_placeholder_run(
            term,
            cache,
            current,
            screen_line,
            cell_width,
            cell_height,
            viewport_width,
            viewport_height,
            draws,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn flush_placeholder_run(
    term: &VtTerminal,
    cache: &KittyRenderCache,
    current: Option<(IncompletePlacement, usize)>,
    screen_line: usize,
    cell_width: f32,
    cell_height: f32,
    viewport_width: f32,
    viewport_height: f32,
    draws: &mut Vec<KittyDraw>,
) {
    let Some((incomplete, start_col)) = current else {
        return;
    };
    let run: PlaceholderRun = incomplete.complete();
    let Some(placement) = term
        .graphics
        .kitty_virtual_placements
        .get(&(run.image_id, run.placement_id))
    else {
        return;
    };
    let Some(image) = cache.images.get(&run.image_id) else {
        return;
    };
    let Some(geometry) = compute_run_geometry(
        &run,
        placement.columns,
        placement.rows,
        image.width,
        image.height,
        (placement.x, placement.y, placement.width, placement.height),
        cell_width,
        cell_height,
        0.0,
        0.0,
        screen_line,
        start_col,
    ) else {
        return;
    };
    let mut draw = KittyDraw {
        image_id: run.image_id,
        x: geometry.x,
        y: geometry.y,
        width: geometry.width,
        height: geometry.height,
        source_rect: geometry.source_rect,
        z_index: placement.z_index,
        depth: 0.0,
    };
    if clip_draw(&mut draw, viewport_width, viewport_height) {
        draws.push(draw);
    }
}

fn clip_draw(draw: &mut KittyDraw, width: f32, height: f32) -> bool {
    if draw.width <= 0.0 || draw.height <= 0.0 {
        return false;
    }
    let x0 = draw.x;
    let y0 = draw.y;
    let x1 = x0 + draw.width;
    let y1 = y0 + draw.height;
    let clipped_x0 = x0.max(0.0);
    let clipped_y0 = y0.max(0.0);
    let clipped_x1 = x1.min(width);
    let clipped_y1 = y1.min(height);
    if clipped_x1 <= clipped_x0 || clipped_y1 <= clipped_y0 {
        return false;
    }
    let [u0, v0, u1, v1] = draw.source_rect;
    let left = (clipped_x0 - x0) / draw.width;
    let right = (clipped_x1 - x0) / draw.width;
    let top = (clipped_y0 - y0) / draw.height;
    let bottom = (clipped_y1 - y0) / draw.height;
    draw.source_rect = [
        u0 + (u1 - u0) * left,
        v0 + (v1 - v0) * top,
        u0 + (u1 - u0) * right,
        v0 + (v1 - v0) * bottom,
    ];
    draw.x = clipped_x0;
    draw.y = clipped_y0;
    draw.width = clipped_x1 - clipped_x0;
    draw.height = clipped_y1 - clipped_y0;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn viewport_clipping_adjusts_geometry_and_uv_together() {
        let mut draw = KittyDraw {
            image_id: 1,
            x: -10.0,
            y: -5.0,
            width: 20.0,
            height: 20.0,
            source_rect: [0.0, 0.0, 1.0, 1.0],
            z_index: 0,
            depth: 0.0,
        };
        assert!(clip_draw(&mut draw, 100.0, 100.0));
        assert_eq!(
            (draw.x, draw.y, draw.width, draw.height),
            (0.0, 0.0, 10.0, 15.0)
        );
        assert_eq!(draw.source_rect, [0.5, 0.25, 1.0, 1.0]);
    }

    #[test]
    fn stacking_uses_z_then_image_id_and_keeps_text_boundary() {
        let draw = |image_id, z_index| KittyDraw {
            image_id,
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
            source_rect: [0.0, 0.0, 1.0, 1.0],
            z_index,
            depth: 0.0,
        };
        let mut draws = vec![draw(9, 0), draw(5, -1), draw(3, 0), draw(1, -2)];
        resolve_stack_depths(&mut draws);

        assert_eq!(
            draws
                .iter()
                .map(|draw| (draw.z_index, draw.image_id))
                .collect::<Vec<_>>(),
            vec![(-2, 1), (-1, 5), (0, 3), (0, 9)]
        );
        assert!(draws.windows(2).all(|pair| pair[0].depth < pair[1].depth));
        assert!(draws[1].depth < 0.0);
        assert!(draws[2].depth > 0.0);
    }
}
