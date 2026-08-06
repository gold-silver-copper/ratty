//! Inline object state and APC handling.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::Path;

use bevy::prelude::*;
use vt100::Callbacks;

use crate::bitmap::{BitmapSurfaceState, MAX_BITMAP_APC_BYTES};
use crate::bitmap_material::{BitmapSurfaceMaterial, BitmapSurfaceUniform};
use crate::camera::{OptionalVec3, TerminalCameraUpdate};
use crate::kitty::{KittyOperation, KittyParserState, refresh_kitty_placeholder_anchors};
use crate::model::{
    ObjectLoadOptions, load_object_source_from_bytes_with_options, load_object_source_with_options,
};
use crate::rgp::{
    RgpOperation, RgpPlacementStyle, RgpPlacementUpdate, RgpRegisterSource,
    consume_sequence as consume_rgp_sequence, support_reply,
};

const APC_START: &[u8] = b"\x1b_";
const ST: &[u8] = b"\x1b\\";
const C1_ST: u8 = 0x9c;

/// Marker for 2D inline object sprites.
#[derive(Component)]
pub struct TerminalInlineObjectSprite;

/// Marker for 3D inline object planes.
#[derive(Component)]
pub struct TerminalInlineObjectPlane;

/// Layout data used to animate Kitty image planes on the warped terminal surface.
#[derive(Component, Clone, Copy)]
pub(crate) struct InlineKittyPlaneLayout {
    /// Normalized horizontal center within the terminal plane.
    pub local_x: f32,
    /// Normalized vertical center within the terminal plane.
    pub local_y: f32,
    /// Normalized width within the terminal plane.
    pub local_width: f32,
    /// Normalized height within the terminal plane.
    pub local_height: f32,
    /// Horizontal mesh subdivision count.
    pub x_segments: u32,
    /// Vertical mesh subdivision count.
    pub y_segments: u32,
}

/// Cached GPU assets for a Kitty image plane attached to the terminal surface.
pub(crate) struct KittyPlaneCache {
    /// Cached horizontal mesh subdivision count.
    pub x_segments: u32,
    /// Cached vertical mesh subdivision count.
    pub y_segments: u32,
    /// Cached plane mesh handle.
    pub mesh: Handle<Mesh>,
    /// Cached plane material handle.
    pub material: Handle<StandardMaterial>,
}

/// Marker for RGP-backed inline objects.
#[derive(Component)]
pub struct TerminalRgpObject {
    /// Registered object identifier.
    pub object_id: u32,
}

/// Marker identifying one rendered bitmap-surface placement.
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct TerminalBitmapPlacement {
    /// Globally unique placement identifier.
    pub placement_id: u32,
    /// Registered bitmap identifier shared by this placement.
    pub bitmap_id: u32,
}

/// Stable Bevy assets owned by one bitmap placement.
pub(crate) struct BitmapPlacementRenderCache {
    /// Placement lifetime rendered by this cache entry.
    pub(crate) generation: u64,
    /// Registered bitmap used by the placement.
    pub(crate) bitmap_id: u32,
    /// Stable render entity.
    pub(crate) entity: Entity,
    /// Stable destination quad mesh.
    pub(crate) mesh: Handle<Mesh>,
    /// Stable per-placement material.
    pub(crate) material: Handle<BitmapSurfaceMaterial>,
    /// Last values synchronized into the stable render objects.
    pub(crate) state: BitmapPlacementRenderState,
}

/// Render-facing placement values used to avoid dirtying unchanged Bevy assets.
#[derive(Clone)]
pub(crate) struct BitmapPlacementRenderState {
    pub(crate) image: Handle<Image>,
    pub(crate) destination: Vec2,
    pub(crate) transform: Transform,
    pub(crate) uniform: BitmapSurfaceUniform,
}

/// Renderer-side identities retained across protocol updates.
#[derive(Default)]
pub(crate) struct BitmapRenderCache {
    /// Stable image handles keyed by bitmap ID.
    pub(crate) images: HashMap<u32, Handle<Image>>,
    /// Stable entity and material handles keyed by placement ID.
    pub(crate) placements: HashMap<u32, BitmapPlacementRenderCache>,
}

/// Inline object registry and anchor state.
#[derive(Resource, Default)]
pub struct TerminalInlineObjects {
    pending_bytes: Vec<u8>,
    discarding_oversized_bitmap_apc: bool,
    bitmap_discard_saw_escape: bool,
    pending_rgp_payloads: HashMap<u32, PendingRgpPayload>,
    kitty: KittyParserState,
    pub(crate) bitmap: BitmapSurfaceState,
    pub(crate) bitmap_render: BitmapRenderCache,
    dirty: bool,
    last_viewport_size: Vec2,
    last_cols: u16,
    last_rows: u16,
    pub(crate) objects: HashMap<u32, InlineObject>,
    pub(crate) anchors: HashMap<u32, InlineAnchor>,
}

impl TerminalInlineObjects {
    pub(crate) fn with_bitmap_limits(limits: crate::bitmap::BitmapLimits) -> Self {
        Self {
            bitmap: BitmapSurfaceState::with_limits(limits),
            ..Self::default()
        }
    }

    /// Consumes PTY output and extracts inline object control sequences.
    pub fn consume_pty_output<CB: Callbacks>(
        &mut self,
        chunk: &[u8],
        parser: &mut vt100::Parser<CB>,
        camera_updates: &mut Vec<TerminalCameraUpdate>,
        terminal_output: &mut bool,
    ) -> Vec<Vec<u8>> {
        self.consume_pty_output_with_limit(
            chunk,
            parser,
            camera_updates,
            terminal_output,
            MAX_BITMAP_APC_BYTES,
        )
    }

    #[cfg(test)]
    fn consume_pty_output_with_bitmap_limit<CB: Callbacks>(
        &mut self,
        chunk: &[u8],
        parser: &mut vt100::Parser<CB>,
        bitmap_apc_limit: usize,
    ) -> Vec<Vec<u8>> {
        let mut camera_updates = Vec::new();
        let mut terminal_output = false;
        self.consume_pty_output_with_limit(
            chunk,
            parser,
            &mut camera_updates,
            &mut terminal_output,
            bitmap_apc_limit,
        )
    }

    fn consume_pty_output_with_limit<CB: Callbacks>(
        &mut self,
        mut chunk: &[u8],
        parser: &mut vt100::Parser<CB>,
        camera_updates: &mut Vec<TerminalCameraUpdate>,
        terminal_output: &mut bool,
        bitmap_apc_limit: usize,
    ) -> Vec<Vec<u8>> {
        const INGEST_BLOCK_BYTES: usize = 64 * 1024;

        let mut replies = Vec::new();
        while !chunk.is_empty() {
            if self.discarding_oversized_bitmap_apc {
                let Some(consumed) = self.discard_oversized_bitmap_bytes(chunk) else {
                    return replies;
                };
                self.discarding_oversized_bitmap_apc = false;
                self.bitmap_discard_saw_escape = false;
                chunk = &chunk[consumed..];
                continue;
            }

            let block_limit = if self
                .pending_bytes
                .starts_with(crate::bitmap::BITMAP_APC_START)
            {
                bitmap_apc_limit.saturating_sub(self.pending_bytes.len())
            } else {
                INGEST_BLOCK_BYTES.min(bitmap_apc_limit.max(1))
            };
            if block_limit == 0 {
                self.begin_oversized_bitmap_discard();
                continue;
            }
            let take = chunk.len().min(INGEST_BLOCK_BYTES).min(block_limit);
            self.pending_bytes.extend_from_slice(&chunk[..take]);
            chunk = &chunk[take..];
            replies.extend(self.process_pending_bytes(parser, camera_updates, terminal_output));

            if self
                .pending_bytes
                .starts_with(crate::bitmap::BITMAP_APC_START)
                && self.pending_bytes.len() >= bitmap_apc_limit
            {
                self.begin_oversized_bitmap_discard();
            }
        }
        replies
    }

    fn process_pending_bytes<CB: Callbacks>(
        &mut self,
        parser: &mut vt100::Parser<CB>,
        camera_updates: &mut Vec<TerminalCameraUpdate>,
        terminal_output: &mut bool,
    ) -> Vec<Vec<u8>> {
        let mut replies = Vec::new();

        let mut cursor = 0;
        loop {
            let Some(start_offset) = self.pending_bytes[cursor..]
                .windows(APC_START.len())
                .position(|window| window == APC_START)
            else {
                let pending_len = self.pending_bytes.len();
                let keep_from = pending_apc_prefix_start(&self.pending_bytes, cursor);
                if cursor < keep_from {
                    *terminal_output = true;
                    parser.process(&normalize_hvp_sequences(
                        &self.pending_bytes[cursor..keep_from],
                    ));
                }
                if keep_from < pending_len {
                    self.pending_bytes.drain(..keep_from);
                } else {
                    self.pending_bytes.clear();
                }
                return replies;
            };
            let start = cursor + start_offset;
            if cursor < start {
                *terminal_output = true;
                parser.process(&normalize_hvp_sequences(&self.pending_bytes[cursor..start]));
            }

            let payload_start = start + APC_START.len();
            let Some(end) = apc_end(&self.pending_bytes, payload_start) else {
                self.pending_bytes.drain(..start);
                return replies;
            };
            let sequence = self.pending_bytes[start..end].to_vec();
            let (handled, reply) = self.handle_apc_sequence(
                &sequence,
                parser.screen().cursor_position(),
                camera_updates,
            );
            if let Some(reply) = reply {
                replies.push(reply);
            }
            if !handled {
                *terminal_output = true;
                parser.process(&sequence);
            }
            cursor = end;
        }
    }

    fn begin_oversized_bitmap_discard(&mut self) {
        warn!("discarding oversized Ratty Bitmap Surface APC sequence");
        self.bitmap_discard_saw_escape = self.pending_bytes.last() == Some(&ST[0]);
        self.pending_bytes.clear();
        self.discarding_oversized_bitmap_apc = true;
    }

    fn discard_oversized_bitmap_bytes(&mut self, bytes: &[u8]) -> Option<usize> {
        for (index, byte) in bytes.iter().copied().enumerate() {
            if self.bitmap_discard_saw_escape && byte == ST[1] {
                return Some(index + 1);
            }
            if byte == C1_ST {
                return Some(index + 1);
            }
            self.bitmap_discard_saw_escape = byte == ST[0];
        }
        None
    }

    /// Returns whether inline objects need synchronization.
    pub fn needs_sync(&self, viewport_size: Vec2, cols: u16, rows: u16) -> bool {
        self.dirty
            || self.bitmap.is_dirty()
            || self.last_viewport_size != viewport_size
            || self.last_cols != cols
            || self.last_rows != rows
    }

    /// Marks synchronization as complete.
    pub fn finish_sync(&mut self, viewport_size: Vec2, cols: u16, rows: u16) {
        self.dirty = false;
        self.bitmap.take_dirty();
        self.last_viewport_size = viewport_size;
        self.last_cols = cols;
        self.last_rows = rows;
    }

    /// Marks only bitmap protocol changes as synchronized.
    pub(crate) fn finish_bitmap_sync(&mut self) {
        self.bitmap.take_dirty();
    }

    /// Applies upward scroll to anchored objects.
    pub fn apply_scroll(&mut self, rows_scrolled: u16) {
        if rows_scrolled == 0
            || (self.anchors.is_empty() && self.bitmap.placements().next().is_none())
        {
            return;
        }

        self.anchors.retain(|object_id, anchor| {
            if self
                .objects
                .get(object_id)
                .is_some_and(|object| !object.scrolls_with_text())
            {
                return true;
            }
            let new_row = anchor.row as i32 - rows_scrolled as i32;
            if new_row + anchor.rows as i32 <= 0 {
                return false;
            }
            anchor.row = new_row.max(0) as u16;
            true
        });
        self.bitmap.apply_scroll(rows_scrolled);
        self.dirty = true;
    }

    /// Returns whether any anchors need scroll tracking.
    pub fn has_scroll_tracked_anchors(&self) -> bool {
        self.bitmap.placements().next().is_some()
            || self.anchors.keys().any(|object_id| {
                self.objects
                    .get(object_id)
                    .is_some_and(InlineObject::scrolls_with_text)
            })
    }

    /// Refreshes placeholder-derived Kitty anchors.
    pub fn refresh_placeholder_anchors(&mut self, screen: &vt100::Screen) {
        if refresh_kitty_placeholder_anchors(&self.objects, &mut self.anchors, screen) {
            self.dirty = true;
        }
    }

    fn set_anchor(&mut self, object_id: u32, anchor: InlineAnchor) {
        self.anchors.insert(object_id, anchor);
        self.dirty = true;
    }

    fn remove_object(&mut self, object_id: u32) {
        self.objects.remove(&object_id);
        self.anchors.remove(&object_id);
        self.pending_rgp_payloads.remove(&object_id);
        self.dirty = true;
    }

    fn clear_objects(&mut self) {
        self.objects.clear();
        self.anchors.clear();
        self.pending_rgp_payloads.clear();
        self.dirty = true;
    }

    fn handle_apc_sequence(
        &mut self,
        sequence: &[u8],
        cursor_position: (u16, u16),
        camera_updates: &mut Vec<TerminalCameraUpdate>,
    ) -> (bool, Option<Vec<u8>>) {
        if let Some(result) = self.bitmap.consume_and_apply(sequence) {
            debug!(bytes = sequence.len(), "received bitmap surface command");
            return match result {
                Ok(Some(reply)) => {
                    info!("bitmap support query answered: v1");
                    (true, Some(reply))
                }
                Ok(None) => (true, None),
                Err(error) => {
                    warn!("failed to apply bitmap surface command: {error}");
                    (true, None)
                }
            };
        }

        if let Some(reply) = self.handle_rgp_sequence(sequence, camera_updates) {
            return (true, reply);
        }

        let Some(operation) = self.kitty.consume_sequence(sequence, cursor_position) else {
            return (false, None);
        };

        match operation {
            KittyOperation::Pending | KittyOperation::Ignored => (true, None),
            KittyOperation::Query {
                image_id,
                result,
                quiet,
            } => {
                let reply = match result {
                    Ok(()) if quiet == 1 => None,
                    Err(_) if quiet == 2 => None,
                    Ok(()) => Some(format!("\x1b_Gi={image_id};OK\x1b\\").into_bytes()),
                    Err(error) => {
                        Some(format!("\x1b_Gi={image_id};EINVAL:{error}\x1b\\").into_bytes())
                    }
                };
                (true, reply)
            }
            KittyOperation::TransmitOnly { object_id, image } => {
                self.objects
                    .insert(object_id, InlineObject::KittyImage(image.rasterize()));
                self.dirty = true;
                (true, None)
            }
            KittyOperation::TransmitAndPlace {
                object_id,
                image,
                anchor,
            } => {
                self.remove_objects_at(&InlineAnchor {
                    row: anchor.row,
                    col: anchor.col,
                    columns: anchor.columns,
                    rows: anchor.rows,
                    style: InlineStyle::default(),
                });
                self.objects
                    .insert(object_id, InlineObject::KittyImage(image.rasterize()));
                self.set_anchor(
                    object_id,
                    InlineAnchor {
                        row: anchor.row,
                        col: anchor.col,
                        columns: anchor.columns,
                        rows: anchor.rows,
                        style: InlineStyle::default(),
                    },
                );
                (true, None)
            }
            KittyOperation::PlaceExisting { object_id, anchor } => {
                if self.objects.contains_key(&object_id) {
                    self.set_anchor(
                        object_id,
                        InlineAnchor {
                            row: anchor.row,
                            col: anchor.col,
                            columns: anchor.columns,
                            rows: anchor.rows,
                            style: InlineStyle::default(),
                        },
                    );
                }
                (true, None)
            }
            KittyOperation::Delete { object_id } => {
                if let Some(object_id) = object_id {
                    self.remove_object(object_id);
                } else {
                    self.clear_objects();
                }
                (true, None)
            }
        }
    }

    fn handle_rgp_sequence(
        &mut self,
        sequence: &[u8],
        camera_updates: &mut Vec<TerminalCameraUpdate>,
    ) -> Option<Option<Vec<u8>>> {
        let operation = consume_rgp_sequence(sequence)?;
        Some(match operation {
            RgpOperation::SupportQuery => Some(support_reply()),
            RgpOperation::Camera {
                camera_slot,
                switch_immediately,
                settings,
            } => {
                camera_updates.push(TerminalCameraUpdate {
                    slot: camera_slot as usize,
                    activate: switch_immediately,
                    mode: settings.camera_type,
                    scale: settings.scale,
                    fov: settings.fov,
                    translation: OptionalVec3::from(settings.offset),
                    rotation_degrees: OptionalVec3::from(settings.rotation),
                });
                None
            }
            RgpOperation::Register {
                object_id,
                format,
                options,
                source,
            } => {
                let load_options = ObjectLoadOptions {
                    normalize: options.normalize,
                };
                if format != "obj" && format != "glb" && format != "stl" {
                    warn!("unsupported RGP object format `{format}` for object {object_id}");
                    None
                } else {
                    match source {
                        RgpRegisterSource::Path { path } => {
                            self.pending_rgp_payloads.remove(&object_id);
                            match load_object_source_with_options(Path::new(&path), load_options) {
                                Ok((source, source_data)) => {
                                    info!("registered RGP object {} from {}", object_id, source);
                                    self.objects.insert(object_id, source_data.into());
                                    self.dirty = true;
                                    None
                                }
                                Err(error) => {
                                    warn!("failed to load RGP object {object_id}: {error:#}");
                                    None
                                }
                            }
                        }
                        RgpRegisterSource::Payload { name, more, data } => self
                            .handle_rgp_payload_chunk(
                                object_id,
                                &format,
                                name,
                                more,
                                data,
                                load_options,
                            ),
                    }
                }
            }
            RgpOperation::Place { object_id, anchor } => {
                if self.objects.contains_key(&object_id) {
                    let row = anchor
                        .row
                        .saturating_sub(anchor.rows.saturating_sub(1).div_ceil(2) as u16);
                    let col = anchor
                        .col
                        .saturating_sub(anchor.columns.saturating_sub(1).div_ceil(2) as u16);
                    self.set_anchor(
                        object_id,
                        InlineAnchor {
                            row,
                            col,
                            columns: anchor.columns,
                            rows: anchor.rows,
                            style: anchor.style.into(),
                        },
                    );
                }
                None
            }
            RgpOperation::Update { object_id, update } => {
                if let Some(anchor) = self.anchors.get_mut(&object_id) {
                    let needs_respawn = update.depth.is_some()
                        || update.color.is_some()
                        || update.brightness.is_some();
                    apply_rgp_update(&mut anchor.style, update);
                    if needs_respawn {
                        self.dirty = true;
                    }
                }
                None
            }
            RgpOperation::Delete { object_id } => {
                if let Some(object_id) = object_id {
                    self.remove_object(object_id);
                } else {
                    self.clear_objects();
                }
                None
            }
            RgpOperation::Ignored => None,
        })
    }

    fn remove_objects_at(&mut self, new_anchor: &InlineAnchor) {
        let row_start = new_anchor.row as i32;
        let row_end = row_start + new_anchor.rows as i32;
        let col_start = new_anchor.col as i32;
        let col_end = col_start + new_anchor.columns as i32;

        let overlapping_ids = self
            .anchors
            .iter()
            .filter_map(|(object_id, anchor)| {
                let anchor_row_start = anchor.row as i32;
                let anchor_row_end = anchor_row_start + anchor.rows as i32;
                let anchor_col_start = anchor.col as i32;
                let anchor_col_end = anchor_col_start + anchor.columns as i32;

                (anchor_row_start < row_end
                    && anchor_row_end > row_start
                    && anchor_col_start < col_end
                    && anchor_col_end > col_start)
                    .then_some(*object_id)
            })
            .collect::<Vec<_>>();

        for object_id in overlapping_ids {
            self.objects.remove(&object_id);
            self.anchors.remove(&object_id);
        }
    }

    // Buffers chunked payload registrations until the final chunk arrives, then loads and registers the object.
    fn handle_rgp_payload_chunk(
        &mut self,
        object_id: u32,
        format: &str,
        name: Option<String>,
        more: bool,
        data: Vec<u8>,
        options: ObjectLoadOptions,
    ) -> Option<Vec<u8>> {
        let pending = self
            .pending_rgp_payloads
            .entry(object_id)
            .or_insert_with(|| PendingRgpPayload {
                format: format.to_string(),
                name: name.clone(),
                data: Vec::new(),
                options,
            });
        if pending.format != format {
            warn!(
                "ignoring RGP payload chunk for object {} due to format mismatch ({} vs {})",
                object_id, pending.format, format
            );
            return None;
        }
        if pending.name.is_none() {
            pending.name = name;
        }
        pending.data.extend_from_slice(&data);
        info!(
            "received RGP payload chunk for object {} (format={}, accumulated={} bytes, more={})",
            object_id,
            pending.format,
            pending.data.len(),
            more
        );
        if more {
            return None;
        }

        let pending = self.pending_rgp_payloads.remove(&object_id)?;
        info!(
            "finalizing RGP payload for object {} (format={}, total={} bytes)",
            object_id,
            pending.format,
            pending.data.len()
        );
        match load_object_source_from_bytes_with_options(
            &pending.format,
            pending.name.as_deref(),
            &pending.data,
            pending.options,
        ) {
            Ok((source, source_data)) => {
                info!("registered RGP object {} from {}", object_id, source);
                self.objects.insert(object_id, source_data.into());
                self.dirty = true;
                None
            }
            Err(error) => {
                warn!("failed to load RGP object {object_id}: {error:#}");
                None
            }
        }
    }
}

struct PendingRgpPayload {
    format: String,
    name: Option<String>,
    data: Vec<u8>,
    options: ObjectLoadOptions,
}

fn normalize_hvp_sequences(bytes: &[u8]) -> Cow<'_, [u8]> {
    // vt100 handles CUP (`H`) but not HVP (`f`), so normalize cursor-positioning sequences.
    let mut normalized = None;
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 2 < bytes.len() && bytes[i + 1] == b'[' {
            let mut j = i + 2;
            while j < bytes.len() && matches!(bytes[j], b'0'..=b'9' | b';') {
                j += 1;
            }

            if j < bytes.len() && bytes[j] == b'f' && j > i + 2 {
                let out = normalized.get_or_insert_with(|| {
                    let mut out = Vec::with_capacity(bytes.len());
                    out.extend_from_slice(&bytes[..i]);
                    out
                });
                out.extend_from_slice(&bytes[i..j]);
                out.push(b'H');
                i = j + 1;
                continue;
            }
        }

        if let Some(out) = normalized.as_mut() {
            out.push(bytes[i]);
        }
        i += 1;
    }

    match normalized {
        Some(bytes) => Cow::Owned(bytes),
        None => Cow::Borrowed(bytes),
    }
}

fn pending_apc_prefix_start(bytes: &[u8], cursor: usize) -> usize {
    let start = cursor.min(bytes.len());
    if bytes[start..].ends_with(&APC_START[..1]) {
        bytes.len() - 1
    } else {
        bytes.len()
    }
}

fn apc_end(bytes: &[u8], payload_start: usize) -> Option<usize> {
    let mut index = payload_start;
    loop {
        if index >= bytes.len() {
            return None;
        }
        if bytes[index] == C1_ST {
            return Some(index + 1);
        }
        if index + 1 < bytes.len() && bytes[index] == ST[0] && bytes[index + 1] == ST[1] {
            return Some(index + 2);
        }
        index += 1;
    }
}

/// Registered inline object.
pub enum InlineObject {
    /// Kitty image object.
    KittyImage(KittyInlineObject),
    /// Ratty graphics object.
    RgpObject(RgpInlineObject),
}

/// Raster image payload.
pub struct RasterObject {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// RGBA image bytes.
    pub rgba: Vec<u8>,
    /// Uploaded image handle.
    pub handle: Option<Handle<Image>>,
}

/// Kitty-backed inline object.
pub struct KittyInlineObject {
    /// Raster image payload.
    pub raster: RasterObject,
    /// Indicates placeholder-driven placement.
    pub uses_placeholders: bool,
    /// Cached plane mesh and material for 3D presentation.
    pub(crate) plane: Option<KittyPlaneCache>,
}

/// RGP-backed inline object.
pub enum RgpInlineObject {
    /// STL mesh payload.
    Stl {
        /// The loaded mesh
        mesh: Mesh,
        /// Cached extruded mesh handle keyed by extrusion depth.
        handle: Option<(u32, Handle<Mesh>)>,
    },
    /// OBJ mesh payload.
    Obj {
        /// Loaded mesh parts.
        meshes: Vec<Mesh>,
        /// Cached mesh handles keyed by depth.
        handles: Option<(u32, Vec<Handle<Mesh>>)>,
    },
    /// glTF scene payload.
    Gltf {
        /// Scene asset path.
        asset_path: String,
        /// Cached scene handle.
        handle: Option<Handle<WorldAsset>>,
    },
}

impl InlineObject {
    fn scrolls_with_text(&self) -> bool {
        match self {
            InlineObject::KittyImage(object) => !object.uses_placeholders,
            InlineObject::RgpObject(_) => true,
        }
    }
}

/// Inline object anchor.
pub struct InlineAnchor {
    /// Anchor row.
    pub row: u16,
    /// Anchor column.
    pub col: u16,
    /// Object width in cells.
    pub columns: u32,
    /// Object height in cells.
    pub rows: u32,
    /// Inline styling.
    pub style: InlineStyle,
}

/// Inline object style.
#[derive(Clone, Copy, Default)]
pub struct InlineStyle {
    /// Enables default animation.
    pub animate: bool,
    /// Scale multiplier.
    pub scale: f32,
    /// Extrusion depth.
    pub depth: f32,
    /// Optional object color.
    pub color: Option<[u8; 3]>,
    /// Brightness multiplier.
    pub brightness: f32,
    /// Translation offset relative to the anchor.
    pub offset: Vec3,
    /// Rotation in degrees.
    pub rotation: Vec3,
    /// Non-uniform scale multiplier.
    pub scale3: Vec3,
}

impl From<RgpPlacementStyle> for InlineStyle {
    fn from(value: RgpPlacementStyle) -> Self {
        Self {
            animate: value.animate,
            scale: value.scale,
            depth: value.depth,
            color: value.color,
            brightness: value.brightness,
            offset: Vec3::from_array(value.offset),
            rotation: Vec3::from_array(value.rotation),
            scale3: Vec3::from_array(value.scale3),
        }
    }
}

fn apply_rgp_update(style: &mut InlineStyle, update: RgpPlacementUpdate) {
    if let Some(animate) = update.animate {
        style.animate = animate;
    }
    if let Some(scale) = update.scale {
        style.scale = scale;
    }
    if let Some(depth) = update.depth {
        style.depth = depth;
    }
    if let Some(color) = update.color {
        style.color = Some(color);
    }
    if let Some(brightness) = update.brightness {
        style.brightness = brightness;
    }
    apply_vec3_update(&mut style.offset, update.offset);
    apply_vec3_update(&mut style.rotation, update.rotation);
    apply_vec3_update(&mut style.scale3, update.scale3);
}

fn apply_vec3_update(target: &mut Vec3, update: [Option<f32>; 3]) {
    if let Some(x) = update[0] {
        target.x = x;
    }
    if let Some(y) = update[1] {
        target.y = y;
    }
    if let Some(z) = update[2] {
        target.z = z;
    }
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;

    use super::*;

    const BITMAP_SUPPORT_REPLY: &[u8] = b"\x1b_ratty;i;s;v=1;fmt=png;frame=rgba8;payload=1;chunk=1;placement=1;crop=1;fit=contain|cover|fill;filter=nearest|linear;opacity=1\x1b\\";
    const RATATUI_IMAGE_KITTY_QUERY: &[u8] = b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\";
    const PNG_2X2: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x08, 0x06, 0x00, 0x00, 0x00, 0x72,
        0xb6, 0x0d, 0x24, 0x00, 0x00, 0x00, 0x12, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8,
        0xcf, 0xc0, 0xf0, 0x1f, 0x0c, 0x81, 0x34, 0x18, 0x00, 0x00, 0x49, 0xc8, 0x09, 0xf7, 0xf9,
        0xab, 0xb6, 0x0d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    fn parser() -> vt100::Parser {
        vt100::Parser::new(24, 80, 0)
    }

    fn consume(
        objects: &mut TerminalInlineObjects,
        chunk: &[u8],
        parser: &mut vt100::Parser,
    ) -> Vec<Vec<u8>> {
        let mut camera_updates = Vec::new();
        let mut terminal_output = false;
        objects.consume_pty_output(chunk, parser, &mut camera_updates, &mut terminal_output)
    }

    fn register_bitmap(objects: &mut TerminalInlineObjects, parser: &mut vt100::Parser) {
        let payload = base64::engine::general_purpose::STANDARD.encode(PNG_2X2);
        let command = format!("\x1b_ratty;i;r;id=7;fmt=png;source=payload;more=0;{payload}\x1b\\");
        assert!(consume(objects, command.as_bytes(), parser).is_empty());
    }

    #[test]
    fn consumes_bitmap_apc_without_leaking_it_into_mixed_terminal_text() {
        let mut objects = TerminalInlineObjects::default();
        let mut parser = parser();

        let replies = consume(&mut objects, b"left\x1b_ratty;i;s\x1b\\right", &mut parser);

        assert_eq!(replies, vec![BITMAP_SUPPORT_REPLY.to_vec()]);
        assert_eq!(parser.screen().contents(), "leftright");
    }

    #[test]
    fn kitty_query_reports_support_without_storing_an_image() {
        let mut objects = TerminalInlineObjects::default();
        let mut parser = vt100::Parser::new(24, 80, 0);

        let replies = consume(&mut objects, RATATUI_IMAGE_KITTY_QUERY, &mut parser);

        assert_eq!(replies, [b"\x1b_Gi=31;OK\x1b\\".to_vec()]);
        assert!(objects.objects.is_empty());
        assert!(objects.anchors.is_empty());
    }

    #[test]
    fn invalid_kitty_query_reports_error_without_mutating_state() {
        let mut objects = TerminalInlineObjects::default();
        let mut parser = vt100::Parser::new(24, 80, 0);

        let replies = consume(
            &mut objects,
            b"\x1b_Gi=9,s=2,v=2,a=q,t=d,f=24;AAAA\x1b\\",
            &mut parser,
        );

        assert_eq!(
            replies,
            [b"\x1b_Gi=9;EINVAL:invalid pixel data\x1b\\".to_vec()]
        );
        assert!(objects.objects.is_empty());
        assert!(objects.anchors.is_empty());
    }

    #[test]
    fn kitty_query_quiet_levels_suppress_the_requested_reply_class() {
        let mut objects = TerminalInlineObjects::default();
        let mut parser = vt100::Parser::new(24, 80, 0);

        let ok_replies = consume(
            &mut objects,
            b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24,q=1;AAAA\x1b\\",
            &mut parser,
        );
        let error_replies = consume(
            &mut objects,
            b"\x1b_Gi=9,s=2,v=2,a=q,t=d,f=24,q=2;AAAA\x1b\\",
            &mut parser,
        );

        assert!(ok_replies.is_empty());
        assert!(error_replies.is_empty());
        assert!(objects.objects.is_empty());
        assert!(objects.anchors.is_empty());
    }

    #[test]
    fn buffers_fragmented_bitmap_apc_until_its_terminator_arrives() {
        let mut objects = TerminalInlineObjects::default();
        let mut parser = parser();

        assert!(consume(&mut objects, b"before\x1b_ratty;i;", &mut parser).is_empty());
        assert_eq!(parser.screen().contents(), "before");
        let replies = consume(&mut objects, b"s\x1b\\after", &mut parser);

        assert_eq!(replies, vec![BITMAP_SUPPORT_REPLY.to_vec()]);
        assert_eq!(parser.screen().contents(), "beforeafter");
    }

    #[test]
    fn bounds_and_discards_oversized_fragmented_bitmap_apc_then_recovers_after_split_st() {
        let mut objects = TerminalInlineObjects::default();
        let mut parser = parser();
        let limit = 32;

        objects.consume_pty_output_with_bitmap_limit(
            b"before\x1b_ratty;i;r;id=1;",
            &mut parser,
            limit,
        );
        objects.consume_pty_output_with_bitmap_limit(b"AAAAAAAAAAAAAAAA", &mut parser, limit);
        assert!(objects.pending_bytes.len() <= limit);
        assert!(objects.discarding_oversized_bitmap_apc);

        objects.consume_pty_output_with_bitmap_limit(b"discarded\x1b", &mut parser, limit);
        let replies = objects.consume_pty_output_with_bitmap_limit(
            b"\\after\x1b_ratty;i;s\x1b\\",
            &mut parser,
            limit,
        );

        assert_eq!(replies, vec![BITMAP_SUPPORT_REPLY.to_vec()]);
        assert_eq!(parser.screen().contents(), "beforeafter");
        assert!(!objects.discarding_oversized_bitmap_apc);
        assert!(objects.pending_bytes.len() <= limit);
    }

    #[test]
    fn discards_oversized_bitmap_apc_until_c1_st_then_recovers() {
        let mut objects = TerminalInlineObjects::default();
        let mut parser = parser();
        let limit = 24;

        objects.consume_pty_output_with_bitmap_limit(
            b"\x1b_ratty;i;r;id=1;AAAAAAAA",
            &mut parser,
            limit,
        );
        let replies = objects.consume_pty_output_with_bitmap_limit(
            b"discarded\x9ctail\x1b_ratty;i;s\x9c",
            &mut parser,
            limit,
        );

        assert_eq!(replies, vec![BITMAP_SUPPORT_REPLY.to_vec()]);
        assert_eq!(parser.screen().contents(), "tail");
        assert!(!objects.discarding_oversized_bitmap_apc);
    }

    #[test]
    fn bitmap_apc_limit_does_not_apply_to_fragmented_rgp_sequences() {
        let mut objects = TerminalInlineObjects::default();
        let mut parser = parser();
        let limit = 8;

        objects.consume_pty_output_with_bitmap_limit(b"\x1b_ratty;g;", &mut parser, limit);
        let replies = objects.consume_pty_output_with_bitmap_limit(b"s\x1b\\", &mut parser, limit);

        assert_eq!(replies, vec![crate::rgp::support_reply()]);
        assert!(parser.screen().contents().is_empty());
    }

    #[test]
    fn accepts_c1_st_for_bitmap_support_query() {
        let mut objects = TerminalInlineObjects::default();
        let mut parser = parser();

        let replies = consume(&mut objects, b"\x1b_ratty;i;s\x9c", &mut parser);

        assert_eq!(replies, vec![BITMAP_SUPPORT_REPLY.to_vec()]);
        assert!(parser.screen().contents().is_empty());
    }

    #[test]
    fn dispatches_adjacent_bitmap_and_rgp_sequences_in_wire_order() {
        let mut objects = TerminalInlineObjects::default();
        let mut parser = parser();

        let replies = consume(
            &mut objects,
            b"\x1b_ratty;i;s\x1b\\\x1b_ratty;g;s\x1b\\",
            &mut parser,
        );

        assert_eq!(
            replies,
            vec![BITMAP_SUPPORT_REPLY.to_vec(), crate::rgp::support_reply()]
        );
        assert!(parser.screen().contents().is_empty());
    }

    #[test]
    fn dispatches_bitmap_before_adjacent_kitty_and_keeps_bitmap_state_isolated() {
        let mut objects = TerminalInlineObjects::default();
        let mut parser = parser();
        register_bitmap(&mut objects, &mut parser);

        let replies = consume(
            &mut objects,
            b"\x1b_ratty;i;s\x1b\\\x1b_Ga=d;\x1b\\\x1b_ratty;g;d\x1b\\",
            &mut parser,
        );

        assert_eq!(replies, vec![BITMAP_SUPPORT_REPLY.to_vec()]);
        assert!(objects.bitmap.bitmap(7).is_some());
        assert!(parser.screen().contents().is_empty());
    }

    #[test]
    fn malformed_bitmap_sequences_are_consumed_without_terminal_output() {
        let mut objects = TerminalInlineObjects::default();
        let mut parser = parser();

        let replies = consume(
            &mut objects,
            b"before\x1b_ratty;i;p;id=broken\x1b\\after",
            &mut parser,
        );

        assert!(replies.is_empty());
        assert_eq!(parser.screen().contents(), "beforeafter");
    }

    #[test]
    fn bitmap_placements_participate_in_dirty_and_scroll_tracking() {
        let mut objects = TerminalInlineObjects::default();
        let mut parser = parser();
        register_bitmap(&mut objects, &mut parser);
        consume(
            &mut objects,
            b"\x1b_ratty;i;p;id=7;pid=9;row=5;col=2;w=8;h=3\x1b\\",
            &mut parser,
        );

        assert!(objects.needs_sync(Vec2::ZERO, 0, 0));
        assert!(objects.has_scroll_tracked_anchors());
        objects.finish_sync(Vec2::ZERO, 0, 0);
        assert!(!objects.needs_sync(Vec2::ZERO, 0, 0));

        objects.apply_scroll(2);

        assert_eq!(
            objects
                .bitmap
                .placement(9)
                .expect("bitmap placement should exist")
                .row(),
            3
        );
        assert!(objects.needs_sync(Vec2::ZERO, 0, 0));
    }
}
