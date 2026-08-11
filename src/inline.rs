//! Inline object state and APC handling.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};

use bevy::prelude::*;

use crate::bitmap::{
    BITMAP_APC_HEADER_ALLOWANCE, BitmapLimits, BitmapOperation, BitmapSurfaceState,
    MAX_BITMAP_APC_BYTES, max_base64_encoded_bytes,
};
use crate::bitmap_material::{BitmapSurfaceMaterial, BitmapSurfaceUniform};
use crate::camera::{OptionalVec3, TerminalCameraUpdate};
use crate::kitty::KittyRenderCache;
use crate::model::{
    ObjectLoadOptions, ObjectSource, load_rgp_object_source_from_bytes_with_options,
    load_rgp_object_source_with_options, object_source_resident_bytes,
    remove_rgp_materialized_source,
};
use crate::paths::runtime_asset_root;
use crate::rgp::{
    RGP_APC_START, RGP_CONTROL_HEADER_LIMIT, RgpOperation, RgpPlacementStyle, RgpPlacementUpdate,
    RgpRegisterSource, consume_sequence_with_payload_limit as consume_rgp_sequence, support_reply,
};
use crate::runtime::TerminalRuntime;
use crate::vt::VtTerminal;
use rio_vt::crosswords::external_placement::{
    ExternalPlacement, ExternalPlacementErasePolicy, ExternalPlacementScreen,
    ExternalPlacementScrollPolicy,
};

const APC_START: &[u8] = b"\x1b_";
const KITTY_APC_START: &[u8] = b"\x1b_G";
const ST: &[u8] = b"\x1b\\";
const C1_ST: u8 = 0x9c;
const BITMAP_PLACEMENT_NAMESPACE: u64 = 1 << 63;
const RGP_PLACEMENT_NAMESPACE: u64 = 1 << 62;
const RGP_INCOMPLETE_TIMEOUT: Duration = Duration::from_secs(10);

fn forwarded_kitty_apc_limit(max_decoded_bytes: u64) -> usize {
    KITTY_APC_START
        .len()
        .saturating_add(BITMAP_APC_HEADER_ALLOWANCE)
        .saturating_add(max_base64_encoded_bytes(max_decoded_bytes))
        .saturating_add(ST.len())
}

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
    /// Signed offset from the terminal surface derived from Kitty z-index.
    pub depth_offset: f32,
    /// Horizontal mesh subdivision count.
    pub x_segments: u32,
    /// Vertical mesh subdivision count.
    pub y_segments: u32,
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
    forwarded_kitty_apc_limit: Option<usize>,
    discarding_oversized_forwarded_kitty_apc: bool,
    forwarded_kitty_discard_saw_escape: bool,
    rgp_apc_limit: Option<usize>,
    discarding_oversized_rgp_apc: bool,
    rgp_discard_saw_escape: bool,
    rgp_max_payload_bytes: Option<usize>,
    rgp_max_pending_bytes: Option<usize>,
    rgp_max_pending_transfers: Option<usize>,
    rgp_max_objects: Option<usize>,
    rgp_max_total_bytes: Option<usize>,
    pending_rgp_payloads: HashMap<u32, PendingRgpPayload>,
    rgp_object_bytes: HashMap<u32, usize>,
    pub(crate) dirty_rgp_objects: HashSet<u32>,
    pub(crate) bitmap: BitmapSurfaceState,
    pub(crate) bitmap_render: BitmapRenderCache,
    pub(crate) kitty_render: KittyRenderCache,
    dirty: bool,
    last_viewport_size: Vec2,
    last_cols: u16,
    last_rows: u16,
    last_external_revision: u64,
    last_external_viewport_top: i64,
    last_external_screen: Option<ExternalPlacementScreen>,
    pub(crate) objects: HashMap<u32, InlineObject>,
    pub(crate) anchors: HashMap<u32, InlineAnchor>,
}

impl TerminalInlineObjects {
    pub(crate) fn with_bitmap_limits(limits: BitmapLimits) -> Self {
        let forwarded_kitty_apc_limit = forwarded_kitty_apc_limit(limits.max_bitmap_bytes);
        let rgp_max_payload_bytes = usize::try_from(limits.max_bitmap_bytes).unwrap_or(usize::MAX);
        let rgp_max_pending_bytes = usize::try_from(limits.max_pending_bytes).unwrap_or(usize::MAX);
        let rgp_apc_limit = RGP_APC_START
            .len()
            .saturating_add(RGP_CONTROL_HEADER_LIMIT)
            .saturating_add(max_base64_encoded_bytes(limits.max_bitmap_bytes))
            .saturating_add(ST.len());
        Self {
            bitmap: BitmapSurfaceState::with_limits(limits),
            forwarded_kitty_apc_limit: Some(forwarded_kitty_apc_limit),
            rgp_apc_limit: Some(rgp_apc_limit),
            rgp_max_payload_bytes: Some(rgp_max_payload_bytes),
            rgp_max_pending_bytes: Some(rgp_max_pending_bytes),
            rgp_max_pending_transfers: Some(limits.max_pending_transfers),
            rgp_max_objects: Some(limits.max_bitmaps),
            rgp_max_total_bytes: Some(
                usize::try_from(limits.max_total_bitmap_bytes).unwrap_or(usize::MAX),
            ),
            ..Self::default()
        }
    }

    /// Consumes PTY output and extracts inline object control sequences.
    pub fn consume_pty_output(
        &mut self,
        chunk: &[u8],
        runtime: &mut TerminalRuntime,
        camera_updates: &mut Vec<TerminalCameraUpdate>,
        terminal_output: &mut bool,
    ) -> Vec<Vec<u8>> {
        self.consume_pty_output_with_limit(
            chunk,
            runtime,
            camera_updates,
            terminal_output,
            MAX_BITMAP_APC_BYTES,
        )
    }

    #[cfg(test)]
    fn consume_pty_output_with_bitmap_limit(
        &mut self,
        chunk: &[u8],
        runtime: &mut TerminalRuntime,
        bitmap_apc_limit: usize,
    ) -> Vec<Vec<u8>> {
        let mut camera_updates = Vec::new();
        let mut terminal_output = false;
        self.consume_pty_output_with_limit(
            chunk,
            runtime,
            &mut camera_updates,
            &mut terminal_output,
            bitmap_apc_limit,
        )
    }

    fn consume_pty_output_with_limit(
        &mut self,
        mut chunk: &[u8],
        runtime: &mut TerminalRuntime,
        camera_updates: &mut Vec<TerminalCameraUpdate>,
        terminal_output: &mut bool,
        bitmap_apc_limit: usize,
    ) -> Vec<Vec<u8>> {
        const INGEST_BLOCK_BYTES: usize = 64 * 1024;
        let forwarded_kitty_apc_limit = self
            .forwarded_kitty_apc_limit
            .unwrap_or(MAX_BITMAP_APC_BYTES);
        let rgp_apc_limit = self.rgp_apc_limit.unwrap_or(MAX_BITMAP_APC_BYTES);

        self.evict_stale_rgp_payloads();

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
            if self.discarding_oversized_forwarded_kitty_apc {
                let Some(consumed) = self.discard_oversized_forwarded_kitty_bytes(chunk) else {
                    return replies;
                };
                self.discarding_oversized_forwarded_kitty_apc = false;
                self.forwarded_kitty_discard_saw_escape = false;
                chunk = &chunk[consumed..];
                continue;
            }
            if self.discarding_oversized_rgp_apc {
                let Some(consumed) = self.discard_oversized_rgp_bytes(chunk) else {
                    return replies;
                };
                self.discarding_oversized_rgp_apc = false;
                self.rgp_discard_saw_escape = false;
                chunk = &chunk[consumed..];
                continue;
            }

            let block_limit = if self
                .pending_bytes
                .starts_with(crate::bitmap::BITMAP_APC_START)
            {
                bitmap_apc_limit.saturating_sub(self.pending_bytes.len())
            } else if self.pending_bytes.starts_with(KITTY_APC_START) {
                forwarded_kitty_apc_limit.saturating_sub(self.pending_bytes.len())
            } else if self.pending_bytes.starts_with(RGP_APC_START) {
                rgp_apc_limit.saturating_sub(self.pending_bytes.len())
            } else {
                INGEST_BLOCK_BYTES
                    .min(bitmap_apc_limit.max(1))
                    .min(forwarded_kitty_apc_limit.max(1))
                    .min(rgp_apc_limit.max(1))
            };
            if block_limit == 0 {
                if self.pending_bytes.starts_with(KITTY_APC_START) {
                    self.begin_oversized_forwarded_kitty_discard();
                    runtime
                        .term
                        .graphics
                        .kitty_chunking_state
                        .clear_incomplete_transfers();
                } else if self.pending_bytes.starts_with(RGP_APC_START) {
                    self.begin_oversized_rgp_discard();
                } else {
                    self.begin_oversized_bitmap_discard();
                }
                continue;
            }
            let take = chunk.len().min(INGEST_BLOCK_BYTES).min(block_limit);
            self.pending_bytes.extend_from_slice(&chunk[..take]);
            chunk = &chunk[take..];
            replies.extend(self.process_pending_bytes(runtime, camera_updates, terminal_output));

            if self
                .pending_bytes
                .starts_with(crate::bitmap::BITMAP_APC_START)
                && self.pending_bytes.len() >= bitmap_apc_limit
            {
                self.begin_oversized_bitmap_discard();
            } else if self.pending_bytes.starts_with(KITTY_APC_START)
                && self.pending_bytes.len() >= forwarded_kitty_apc_limit
            {
                self.begin_oversized_forwarded_kitty_discard();
                runtime
                    .term
                    .graphics
                    .kitty_chunking_state
                    .clear_incomplete_transfers();
            } else if self.pending_bytes.starts_with(RGP_APC_START)
                && self.pending_bytes.len() >= rgp_apc_limit
            {
                self.begin_oversized_rgp_discard();
            }
        }
        replies
    }

    fn process_pending_bytes(
        &mut self,
        runtime: &mut TerminalRuntime,
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
                    let terminal_bytes = self.pending_bytes[cursor..keep_from].to_vec();
                    self.forward_to_terminal(&terminal_bytes, runtime, &mut replies);
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
                let terminal_bytes = self.pending_bytes[cursor..start].to_vec();
                self.forward_to_terminal(&terminal_bytes, runtime, &mut replies);
            }

            let payload_start = start + APC_START.len();
            let Some(end) = apc_end(&self.pending_bytes, payload_start) else {
                self.pending_bytes.drain(..start);
                return replies;
            };
            let sequence = self.pending_bytes[start..end].to_vec();
            let (handled, reply) = self.handle_apc_sequence(&sequence, runtime, camera_updates);
            if let Some(reply) = reply {
                replies.push(reply);
            }
            if !handled {
                *terminal_output = true;
                self.forward_to_terminal(&sequence, runtime, &mut replies);
            }
            cursor = end;
        }
    }

    fn forward_to_terminal(
        &mut self,
        bytes: &[u8],
        runtime: &mut TerminalRuntime,
        replies: &mut Vec<Vec<u8>>,
    ) {
        runtime.process(bytes);
        replies.extend(runtime.take_replies());
        self.kitty_render
            .queue_updates(runtime.take_graphics_updates());
        self.dirty = true;
    }

    fn begin_oversized_bitmap_discard(&mut self) {
        warn!("discarding oversized Ratty Bitmap Surface APC sequence");
        self.bitmap.abort_all_pending_transfers();
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

    fn begin_oversized_forwarded_kitty_discard(&mut self) {
        warn!("discarding oversized forwarded Kitty APC sequence");
        self.forwarded_kitty_discard_saw_escape = self.pending_bytes.last() == Some(&ST[0]);
        self.pending_bytes.clear();
        self.discarding_oversized_forwarded_kitty_apc = true;
    }

    fn discard_oversized_forwarded_kitty_bytes(&mut self, bytes: &[u8]) -> Option<usize> {
        for (index, byte) in bytes.iter().copied().enumerate() {
            if self.forwarded_kitty_discard_saw_escape && byte == ST[1] {
                return Some(index + 1);
            }
            if byte == C1_ST {
                return Some(index + 1);
            }
            self.forwarded_kitty_discard_saw_escape = byte == ST[0];
        }
        None
    }

    fn begin_oversized_rgp_discard(&mut self) {
        warn!("discarding oversized Ratty Graphics Protocol APC sequence");
        self.pending_rgp_payloads.clear();
        self.rgp_discard_saw_escape = self.pending_bytes.last() == Some(&ST[0]);
        self.pending_bytes.clear();
        self.discarding_oversized_rgp_apc = true;
    }

    fn discard_oversized_rgp_bytes(&mut self, bytes: &[u8]) -> Option<usize> {
        for (index, byte) in bytes.iter().copied().enumerate() {
            if self.rgp_discard_saw_escape && byte == ST[1] {
                return Some(index + 1);
            }
            if byte == C1_ST {
                return Some(index + 1);
            }
            self.rgp_discard_saw_escape = byte == ST[0];
        }
        None
    }

    /// Returns whether inline objects need synchronization.
    pub fn needs_sync(&self, viewport_size: Vec2, cols: u16, rows: u16, term: &VtTerminal) -> bool {
        let viewport_top = external_viewport_top(term);
        self.dirty
            || self.bitmap.is_dirty()
            || self.kitty_render.is_dirty()
            || self.last_viewport_size != viewport_size
            || self.last_cols != cols
            || self.last_rows != rows
            || self.last_external_revision != term.external_placements_revision()
            || self.last_external_viewport_top != viewport_top
            || self.last_external_screen != Some(term.active_external_placement_screen())
    }

    /// Marks synchronization as complete.
    pub fn finish_sync(&mut self, viewport_size: Vec2, cols: u16, rows: u16, term: &VtTerminal) {
        self.dirty = false;
        self.dirty_rgp_objects.clear();
        self.bitmap.take_dirty();
        self.kitty_render.take_dirty();
        self.last_viewport_size = viewport_size;
        self.last_cols = cols;
        self.last_rows = rows;
        self.last_external_revision = term.external_placements_revision();
        self.last_external_viewport_top = external_viewport_top(term);
        self.last_external_screen = Some(term.active_external_placement_screen());
    }

    /// Marks only bitmap protocol changes as synchronized.
    pub(crate) fn finish_bitmap_sync(&mut self) {
        self.bitmap.take_dirty();
    }

    /// Drop renderer-side placement state after the terminal has expired or
    /// reset its authoritative placement record.
    pub(crate) fn reconcile_terminal_placements(&mut self, term: &VtTerminal) {
        let terminal_ids = [
            ExternalPlacementScreen::Main,
            ExternalPlacementScreen::Alternate,
        ]
        .into_iter()
        .flat_map(|screen| {
            term.external_placements(screen)
                .map(|placement| placement.id)
        })
        .collect::<std::collections::HashSet<_>>();

        let stale_bitmaps = self
            .bitmap
            .placements()
            .filter_map(|(placement_id, _)| {
                (!terminal_ids.contains(&bitmap_external_id(*placement_id)))
                    .then_some(*placement_id)
            })
            .collect::<Vec<_>>();
        for placement_id in stale_bitmaps {
            let _ = self
                .bitmap
                .apply(BitmapOperation::DeletePlacement(placement_id));
        }

        let before = self.anchors.len();
        self.anchors
            .retain(|object_id, _| terminal_ids.contains(&rgp_external_id(*object_id)));
        self.dirty |= self.anchors.len() != before;
    }

    fn set_anchor(&mut self, object_id: u32, anchor: InlineAnchor) {
        self.anchors.insert(object_id, anchor);
        self.dirty = true;
    }

    fn remove_object(&mut self, object_id: u32) {
        if let Some(object) = self.objects.remove(&object_id) {
            remove_materialized_inline_object(&object, None);
        }
        self.rgp_object_bytes.remove(&object_id);
        self.anchors.remove(&object_id);
        self.pending_rgp_payloads.remove(&object_id);
        self.dirty_rgp_objects.insert(object_id);
        self.dirty = true;
    }

    fn clear_objects(&mut self) {
        for (_, object) in self.objects.drain() {
            remove_materialized_inline_object(&object, None);
        }
        self.rgp_object_bytes.clear();
        self.anchors.clear();
        self.pending_rgp_payloads.clear();
        self.dirty_rgp_objects.clear();
        self.dirty = true;
    }

    fn handle_apc_sequence(
        &mut self,
        sequence: &[u8],
        runtime: &mut TerminalRuntime,
        camera_updates: &mut Vec<TerminalCameraUpdate>,
    ) -> (bool, Option<Vec<u8>>) {
        if let Some(result) = self.bitmap.consume_apply_and_return(sequence) {
            debug!(bytes = sequence.len(), "received bitmap surface command");
            return match result {
                Ok((operation, reply)) => {
                    self.sync_bitmap_operation(&operation, &mut runtime.term);
                    if reply.is_some() {
                        info!("bitmap support query answered: v1");
                    }
                    (true, reply)
                }
                Err(error) => {
                    warn!("failed to apply bitmap surface command: {error}");
                    (true, None)
                }
            };
        }

        if let Some(reply) = self.handle_rgp_sequence(sequence, camera_updates, &mut runtime.term) {
            return (true, reply);
        }
        // Kitty and every other terminal APC are forwarded byte-for-byte to
        // rio-vt, which owns framing, chunk state, replies, image lifetime,
        // placement mutation, and Unicode-placeholder metadata.
        (false, None)
    }

    pub(crate) fn sync_bitmap_operation(
        &mut self,
        operation: &BitmapOperation,
        term: &mut VtTerminal,
    ) {
        match operation {
            BitmapOperation::Place(placement) => {
                self.register_bitmap_placement(placement.placement_id, true, true, term);
            }
            BitmapOperation::Update {
                placement_id,
                update,
            } if update.row.is_some()
                || update.col.is_some()
                || update.columns.is_some()
                || update.rows.is_some() =>
            {
                self.register_bitmap_placement(
                    *placement_id,
                    update.row.is_some(),
                    update.columns.is_some(),
                    term,
                );
            }
            BitmapOperation::DeletePlacement(placement_id) => {
                remove_external_from_both_screens(term, bitmap_external_id(*placement_id));
            }
            BitmapOperation::DeleteBitmap(_) => {
                for screen in [
                    ExternalPlacementScreen::Main,
                    ExternalPlacementScreen::Alternate,
                ] {
                    let stale = term
                        .external_placements(screen)
                        .filter_map(|placement| {
                            bitmap_placement_id(placement.id).filter(|placement_id| {
                                self.bitmap.placement(*placement_id).is_none()
                            })
                        })
                        .collect::<Vec<_>>();
                    for placement_id in stale {
                        term.remove_external_placement(screen, bitmap_external_id(placement_id));
                    }
                }
            }
            _ => {}
        }
    }

    fn register_bitmap_placement(
        &self,
        placement_id: u32,
        row_was_explicit: bool,
        span_was_explicit: bool,
        term: &mut VtTerminal,
    ) {
        let Some(placement) = self.bitmap.placement(placement_id) else {
            return;
        };
        let id = bitmap_external_id(placement_id);
        let active_screen = term.active_external_placement_screen();
        let existing = [
            ExternalPlacementScreen::Main,
            ExternalPlacementScreen::Alternate,
        ]
        .into_iter()
        .find_map(|screen| {
            term.external_placement(screen, id)
                .cloned()
                .map(|placement| (screen, placement))
        });
        if !row_was_explicit && !span_was_explicit {
            let Some((_screen, mut existing)) = existing else {
                return;
            };
            existing.col = usize::from(placement.col());
            if existing.col.checked_add(existing.columns).is_none() {
                warn!(
                    placement_id,
                    "bitmap placement column overflows terminal coordinates"
                );
                return;
            }
            term.register_external_placement(existing);
            return;
        }

        let (screen, abs_row) = if row_was_explicit {
            (
                active_screen,
                term.external_placement_absolute_row(placement.row()),
            )
        } else {
            let Some((screen, existing)) = existing else {
                return;
            };
            let Ok(source_row) = i64::try_from(existing.source_row) else {
                warn!(
                    placement_id,
                    "bitmap placement source row does not fit signed coordinates"
                );
                return;
            };
            (screen, existing.abs_row.saturating_sub(source_row))
        };
        let Some(external) = ExternalPlacement::new(
            id,
            screen,
            abs_row,
            usize::from(placement.col()),
            placement.columns() as usize,
            placement.rows() as usize,
            ExternalPlacementScrollPolicy::Content,
            ExternalPlacementErasePolicy::Preserve,
        ) else {
            warn!(
                placement_id,
                "bitmap placement does not fit terminal coordinates"
            );
            return;
        };
        if row_was_explicit {
            remove_external_from_both_screens(term, id);
        }
        term.register_external_placement(external);
    }

    fn handle_rgp_sequence(
        &mut self,
        sequence: &[u8],
        camera_updates: &mut Vec<TerminalCameraUpdate>,
        term: &mut VtTerminal,
    ) -> Option<Option<Vec<u8>>> {
        self.evict_stale_rgp_payloads();
        let defaults = BitmapLimits::default();
        let max_payload_bytes = self
            .rgp_max_payload_bytes
            .unwrap_or_else(|| usize::try_from(defaults.max_bitmap_bytes).unwrap_or(usize::MAX));
        let operation = consume_rgp_sequence(sequence, max_payload_bytes)?;
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
                            if !self.can_register_rgp_id(object_id) {
                                warn!(object_id, "RGP object count limit reached");
                                None
                            } else {
                                match load_rgp_object_source_with_options(
                                    object_id,
                                    Path::new(&path),
                                    load_options,
                                    max_payload_bytes,
                                ) {
                                    Ok((source, source_data, encoded_bytes, resident_bytes)) => {
                                        if self.register_rgp_object(
                                            object_id,
                                            source_data,
                                            encoded_bytes,
                                            resident_bytes,
                                        ) {
                                            info!(
                                                "registered RGP object {} from {}",
                                                object_id, source
                                            );
                                        }
                                        None
                                    }
                                    Err(error) => {
                                        warn!("failed to load RGP object {object_id}: {error:#}");
                                        None
                                    }
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
                    let row = i64::from(anchor.row)
                        - i64::from(anchor.rows.saturating_sub(1).div_ceil(2));
                    let col = usize::from(anchor.col).saturating_sub(
                        usize::try_from(anchor.columns.saturating_sub(1).div_ceil(2))
                            .unwrap_or(usize::MAX),
                    );
                    self.set_anchor(
                        object_id,
                        InlineAnchor {
                            style: anchor.style.into(),
                        },
                    );
                    let abs_row = term.external_placement_absolute_row(row);
                    if let Some(placement) = ExternalPlacement::new(
                        rgp_external_id(object_id),
                        term.active_external_placement_screen(),
                        abs_row,
                        col,
                        anchor.columns as usize,
                        anchor.rows as usize,
                        ExternalPlacementScrollPolicy::Content,
                        ExternalPlacementErasePolicy::Preserve,
                    ) {
                        remove_external_from_both_screens(term, rgp_external_id(object_id));
                        term.register_external_placement(placement);
                    }
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
                        self.dirty_rgp_objects.insert(object_id);
                        self.dirty = true;
                    }
                }
                None
            }
            RgpOperation::Delete { object_id } => {
                if let Some(object_id) = object_id {
                    self.remove_object(object_id);
                    remove_external_from_both_screens(term, rgp_external_id(object_id));
                } else {
                    self.clear_objects();
                    for screen in [
                        ExternalPlacementScreen::Main,
                        ExternalPlacementScreen::Alternate,
                    ] {
                        let ids = term
                            .external_placements(screen)
                            .filter(|placement| is_rgp_external_id(placement.id))
                            .map(|placement| placement.id)
                            .collect::<Vec<_>>();
                        for id in ids {
                            term.remove_external_placement(screen, id);
                        }
                    }
                }
                None
            }
            RgpOperation::Ignored => None,
        })
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
        self.evict_stale_rgp_payloads();
        let defaults = BitmapLimits::default();
        let max_payload_bytes = self
            .rgp_max_payload_bytes
            .unwrap_or_else(|| usize::try_from(defaults.max_bitmap_bytes).unwrap_or(usize::MAX));
        let max_pending_bytes = self
            .rgp_max_pending_bytes
            .unwrap_or_else(|| usize::try_from(defaults.max_pending_bytes).unwrap_or(usize::MAX));
        let max_pending_transfers = self
            .rgp_max_pending_transfers
            .unwrap_or(defaults.max_pending_transfers);

        if data.len() > max_payload_bytes {
            self.pending_rgp_payloads.remove(&object_id);
            warn!(object_id, "RGP payload chunk exceeds configured byte limit");
            return None;
        }
        if !self.pending_rgp_payloads.contains_key(&object_id)
            && self.pending_rgp_payloads.len() >= max_pending_transfers
        {
            warn!(object_id, "too many incomplete RGP payload transfers");
            return None;
        }
        let previous_len = self
            .pending_rgp_payloads
            .get(&object_id)
            .map_or(0, |pending| pending.data.len());
        let Some(next_len) = previous_len.checked_add(data.len()) else {
            self.pending_rgp_payloads.remove(&object_id);
            return None;
        };
        let total_without_current = self
            .pending_rgp_payload_bytes()
            .saturating_sub(previous_len);
        if next_len > max_payload_bytes
            || total_without_current.saturating_add(next_len) > max_pending_bytes
        {
            self.pending_rgp_payloads.remove(&object_id);
            warn!(object_id, "RGP pending payload byte limit exceeded");
            return None;
        }

        let pending = self
            .pending_rgp_payloads
            .entry(object_id)
            .or_insert_with(|| PendingRgpPayload {
                format: format.to_string(),
                name: name.clone(),
                data: Vec::new(),
                options,
                last_touched: Instant::now(),
            });
        if pending.format != format {
            warn!(
                "ignoring RGP payload chunk for object {} due to format mismatch ({} vs {})",
                object_id, pending.format, format
            );
            self.pending_rgp_payloads.remove(&object_id);
            return None;
        }
        if pending.name.is_none() {
            pending.name = name;
        }
        if pending.data.try_reserve(data.len()).is_err() {
            self.pending_rgp_payloads.remove(&object_id);
            warn!(object_id, "failed to reserve bounded RGP payload storage");
            return None;
        }
        pending.data.extend_from_slice(&data);
        pending.last_touched = Instant::now();
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
        if !self.can_register_rgp_id(object_id) {
            warn!(object_id, "RGP object count limit reached");
            return None;
        }
        match load_rgp_object_source_from_bytes_with_options(
            object_id,
            &pending.format,
            pending.name.as_deref(),
            &pending.data,
            pending.options,
            max_payload_bytes,
        ) {
            Ok((source, source_data, resident_bytes)) => {
                if self.register_rgp_object(
                    object_id,
                    source_data,
                    pending.data.len(),
                    resident_bytes,
                ) {
                    info!("registered RGP object {} from {}", object_id, source);
                }
                None
            }
            Err(error) => {
                warn!("failed to load RGP object {object_id}: {error:#}");
                None
            }
        }
    }

    fn pending_rgp_payload_bytes(&self) -> usize {
        self.pending_rgp_payloads
            .values()
            .map(|pending| pending.data.len())
            .fold(0usize, usize::saturating_add)
    }

    fn can_register_rgp_id(&self, object_id: u32) -> bool {
        let max_objects = self
            .rgp_max_objects
            .unwrap_or_else(|| BitmapLimits::default().max_bitmaps);
        self.objects.contains_key(&object_id) || self.objects.len() < max_objects
    }

    fn register_rgp_object(
        &mut self,
        object_id: u32,
        source: ObjectSource,
        encoded_bytes: usize,
        decoded_bytes: usize,
    ) -> bool {
        let defaults = BitmapLimits::default();
        let max_object_bytes = self
            .rgp_max_payload_bytes
            .unwrap_or_else(|| usize::try_from(defaults.max_bitmap_bytes).unwrap_or(usize::MAX));
        let max_total_bytes = self.rgp_max_total_bytes.unwrap_or_else(|| {
            usize::try_from(defaults.max_total_bitmap_bytes).unwrap_or(usize::MAX)
        });
        let resident_bytes = decoded_bytes
            .max(object_source_resident_bytes(&source))
            .max(encoded_bytes);
        let previous_bytes = self.rgp_object_bytes.get(&object_id).copied().unwrap_or(0);
        let current_bytes = self
            .rgp_object_bytes
            .values()
            .copied()
            .fold(0usize, usize::saturating_add);
        let next_total = current_bytes
            .saturating_sub(previous_bytes)
            .saturating_add(resident_bytes);
        if resident_bytes > max_object_bytes
            || next_total > max_total_bytes
            || !self.can_register_rgp_id(object_id)
        {
            warn!(
                object_id,
                resident_bytes, next_total, "RGP resident object budget exceeded"
            );
            remove_rgp_materialized_source(&source);
            return false;
        }

        let replacement_path = match &source {
            ObjectSource::Gltf(path) => Some(path.clone()),
            _ => None,
        };
        if let Some(previous) = self.objects.insert(object_id, source.into()) {
            remove_materialized_inline_object(&previous, replacement_path.as_deref());
        }
        self.rgp_object_bytes.insert(object_id, resident_bytes);
        self.dirty_rgp_objects.insert(object_id);
        self.dirty = true;
        true
    }

    fn evict_stale_rgp_payloads(&mut self) {
        let now = Instant::now();
        self.pending_rgp_payloads.retain(|object_id, pending| {
            let keep =
                now.saturating_duration_since(pending.last_touched) <= RGP_INCOMPLETE_TIMEOUT;
            if !keep {
                warn!(
                    object_id = *object_id,
                    "discarding stale incomplete RGP payload transfer"
                );
            }
            keep
        });
    }
}

pub(crate) fn bitmap_external_id(placement_id: u32) -> u64 {
    BITMAP_PLACEMENT_NAMESPACE | u64::from(placement_id)
}

fn bitmap_placement_id(id: u64) -> Option<u32> {
    ((id & BITMAP_PLACEMENT_NAMESPACE) != 0)
        .then(|| u32::try_from(id & u64::from(u32::MAX)).ok())
        .flatten()
}

pub(crate) fn rgp_external_id(object_id: u32) -> u64 {
    RGP_PLACEMENT_NAMESPACE | u64::from(object_id)
}

fn is_rgp_external_id(id: u64) -> bool {
    id & (BITMAP_PLACEMENT_NAMESPACE | RGP_PLACEMENT_NAMESPACE) == RGP_PLACEMENT_NAMESPACE
}

fn remove_external_from_both_screens(term: &mut VtTerminal, id: u64) {
    term.remove_external_placement(ExternalPlacementScreen::Main, id);
    term.remove_external_placement(ExternalPlacementScreen::Alternate, id);
}

fn external_viewport_top(term: &VtTerminal) -> i64 {
    term.external_placement_absolute_row(0)
        .saturating_sub(i64::try_from(term.display_offset()).unwrap_or(i64::MAX))
}

struct PendingRgpPayload {
    format: String,
    name: Option<String>,
    data: Vec<u8>,
    options: ObjectLoadOptions,
    last_touched: Instant,
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
    /// Ratty graphics object.
    RgpObject(RgpInlineObject),
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

fn remove_materialized_inline_object(object: &InlineObject, preserve: Option<&str>) {
    let InlineObject::RgpObject(RgpInlineObject::Gltf { asset_path, .. }) = object else {
        return;
    };
    if preserve == Some(asset_path.as_str()) || !Path::new(asset_path).starts_with("objects/rgp") {
        return;
    }
    let _ = std::fs::remove_file(runtime_asset_root().join(asset_path));
}

/// Renderer-only style for a terminal-owned RGP placement.
pub struct InlineAnchor {
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
    use crate::vt;

    const BITMAP_SUPPORT_REPLY: &[u8] = b"\x1b_ratty;i;s;v=1;fmt=png;frame=rgba8;payload=1;chunk=1;placement=1;crop=1;fit=contain|cover|fill;filter=nearest|linear;opacity=1\x1b\\";

    fn minimal_rgp_glb(marker: u8) -> Vec<u8> {
        let mut bin = Vec::new();
        for value in [0.0f32, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0] {
            bin.extend_from_slice(&value.to_le_bytes());
        }
        for value in [0.0f32, 0.0, 1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 1.0] {
            bin.extend_from_slice(&value.to_le_bytes());
        }
        for index in [0u16, 1, 2] {
            bin.extend_from_slice(&index.to_le_bytes());
        }
        let bin_len = bin.len();
        while bin.len() % 4 != 0 {
            bin.push(0);
        }

        let mut json = format!(
            r#"{{"asset":{{"version":"2.0","generator":"ratty-test-{marker}"}},"scene":0,"scenes":[{{"nodes":[0]}}],"nodes":[{{"mesh":0}}],"meshes":[{{"primitives":[{{"attributes":{{"POSITION":0,"NORMAL":1}},"indices":2}}]}}],"buffers":[{{"byteLength":{bin_len}}}],"bufferViews":[{{"buffer":0,"byteOffset":0,"byteLength":36,"target":34962}},{{"buffer":0,"byteOffset":36,"byteLength":36,"target":34962}},{{"buffer":0,"byteOffset":72,"byteLength":6,"target":34963}}],"accessors":[{{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[0,0,0],"max":[1,1,0]}},{{"bufferView":1,"componentType":5126,"count":3,"type":"VEC3"}},{{"bufferView":2,"componentType":5123,"count":3,"type":"SCALAR"}}]}}"#
        )
        .into_bytes();
        while json.len() % 4 != 0 {
            json.push(b' ');
        }

        let total_len = 12usize
            .checked_add(8 + json.len())
            .and_then(|len| len.checked_add(8 + bin.len()))
            .expect("test GLB length should fit");
        let mut glb = Vec::with_capacity(total_len);
        glb.extend_from_slice(b"glTF");
        glb.extend_from_slice(&2u32.to_le_bytes());
        glb.extend_from_slice(
            &u32::try_from(total_len)
                .expect("test GLB length should fit u32")
                .to_le_bytes(),
        );
        glb.extend_from_slice(
            &u32::try_from(json.len())
                .expect("test JSON length should fit u32")
                .to_le_bytes(),
        );
        glb.extend_from_slice(&0x4E4F_534Au32.to_le_bytes());
        glb.extend_from_slice(&json);
        glb.extend_from_slice(
            &u32::try_from(bin.len())
                .expect("test BIN length should fit u32")
                .to_le_bytes(),
        );
        glb.extend_from_slice(&0x004E_4942u32.to_le_bytes());
        glb.extend_from_slice(&bin);
        glb
    }
    const RATATUI_IMAGE_KITTY_QUERY: &[u8] = b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\";
    const PNG_2X2: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x08, 0x06, 0x00, 0x00, 0x00, 0x72,
        0xb6, 0x0d, 0x24, 0x00, 0x00, 0x00, 0x12, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0xf8,
        0xcf, 0xc0, 0xf0, 0x1f, 0x0c, 0x81, 0x34, 0x18, 0x00, 0x00, 0x49, 0xc8, 0x09, 0xf7, 0xf9,
        0xab, 0xb6, 0x0d, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    fn parser() -> TerminalRuntime {
        TerminalRuntime::for_test(24, 80)
    }

    fn contents(runtime: &TerminalRuntime) -> String {
        vt::visible_row_texts(&runtime.term)
            .join("\n")
            .trim_end()
            .to_owned()
    }

    fn consume(
        objects: &mut TerminalInlineObjects,
        chunk: &[u8],
        parser: &mut TerminalRuntime,
    ) -> Vec<Vec<u8>> {
        let mut camera_updates = Vec::new();
        let mut terminal_output = false;
        objects.consume_pty_output(chunk, parser, &mut camera_updates, &mut terminal_output)
    }

    fn register_bitmap(objects: &mut TerminalInlineObjects, parser: &mut TerminalRuntime) {
        let payload = base64::engine::general_purpose::STANDARD.encode(PNG_2X2);
        let command = format!("\x1b_ratty;i;r;id=7;fmt=png;source=payload;more=0;{payload}\x1b\\");
        assert!(consume(objects, command.as_bytes(), parser).is_empty());
    }

    fn scroll_one_row() -> Vec<u8> {
        b"x\r\n".repeat(24)
    }

    #[test]
    fn consumes_bitmap_apc_without_leaking_it_into_mixed_terminal_text() {
        let mut objects = TerminalInlineObjects::default();
        let mut parser = parser();

        let replies = consume(&mut objects, b"left\x1b_ratty;i;s\x1b\\right", &mut parser);

        assert_eq!(replies, vec![BITMAP_SUPPORT_REPLY.to_vec()]);
        assert_eq!(contents(&parser), "leftright");
    }

    #[test]
    fn kitty_query_reports_support_without_storing_an_image() {
        let mut objects = TerminalInlineObjects::default();
        let mut parser = parser();

        let replies = consume(&mut objects, RATATUI_IMAGE_KITTY_QUERY, &mut parser);

        assert_eq!(replies, [b"\x1b_Gi=31;OK\x1b\\".to_vec()]);
        assert!(objects.objects.is_empty());
        assert!(objects.anchors.is_empty());
    }

    #[test]
    fn invalid_kitty_query_reports_error_without_mutating_state() {
        let mut objects = TerminalInlineObjects::default();
        let mut parser = parser();

        let replies = consume(
            &mut objects,
            b"\x1b_Gi=9,s=2,v=2,a=q,t=d,f=24;AAAA\x1b\\",
            &mut parser,
        );

        assert_eq!(replies, [b"\x1b_Gi=9;EINVAL: invalid data\x1b\\".to_vec()]);
        assert!(objects.objects.is_empty());
        assert!(objects.anchors.is_empty());
    }

    #[test]
    fn kitty_query_quiet_levels_suppress_the_requested_reply_class() {
        let mut objects = TerminalInlineObjects::default();
        let mut parser = parser();

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
        let fully_quiet_ok_replies = consume(
            &mut objects,
            b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24,q=2;AAAA\x1b\\",
            &mut parser,
        );

        assert!(ok_replies.is_empty());
        assert!(error_replies.is_empty());
        assert!(fully_quiet_ok_replies.is_empty());
        assert!(objects.objects.is_empty());
        assert!(objects.anchors.is_empty());
    }

    #[test]
    fn buffers_fragmented_bitmap_apc_until_its_terminator_arrives() {
        let mut objects = TerminalInlineObjects::default();
        let mut parser = parser();

        assert!(consume(&mut objects, b"before\x1b_ratty;i;", &mut parser).is_empty());
        assert_eq!(contents(&parser), "before");
        let replies = consume(&mut objects, b"s\x1b\\after", &mut parser);

        assert_eq!(replies, vec![BITMAP_SUPPORT_REPLY.to_vec()]);
        assert_eq!(contents(&parser), "beforeafter");
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
        assert_eq!(contents(&parser), "beforeafter");
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
        assert_eq!(contents(&parser), "tail");
        assert!(!objects.discarding_oversized_bitmap_apc);
    }

    #[test]
    fn oversized_bitmap_middle_chunk_aborts_pending_registration() {
        let mut objects = TerminalInlineObjects::default();
        let mut parser = parser();
        objects.consume_pty_output_with_bitmap_limit(
            b"\x1b_ratty;i;r;id=41;fmt=png;source=payload;more=1;AQID\x1b\\",
            &mut parser,
            128,
        );

        objects.consume_pty_output_with_bitmap_limit(
            format!("\x1b_ratty;i;r;id=41;more=1;{}\x1b\\", "A".repeat(128)).as_bytes(),
            &mut parser,
            40,
        );
        objects.consume_pty_output_with_bitmap_limit(
            b"\x1b_ratty;i;r;id=41;more=0;BA==\x1b\\",
            &mut parser,
            128,
        );

        assert!(objects.bitmap.bitmap(41).is_none());
    }

    #[test]
    fn bounds_forwarded_kitty_apc_and_recovers_after_split_st() {
        let mut objects = TerminalInlineObjects {
            forwarded_kitty_apc_limit: Some(40),
            ..TerminalInlineObjects::default()
        };
        let mut parser = parser();

        let replies = consume(
            &mut objects,
            b"before\x1b_Ga=t,f=24,s=1,v=1;AAAAAAAAAAAAAAAAAAAAAAAA",
            &mut parser,
        );
        assert!(replies.is_empty());
        assert!(objects.pending_bytes.len() <= 40);
        assert!(objects.discarding_oversized_forwarded_kitty_apc);
        assert_eq!(contents(&parser), "before");

        assert!(consume(&mut objects, b"discarded\x1b", &mut parser).is_empty());
        let replies = consume(
            &mut objects,
            b"\\after\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\",
            &mut parser,
        );

        assert_eq!(replies, [b"\x1b_Gi=31;OK\x1b\\".to_vec()]);
        assert_eq!(contents(&parser), "beforeafter");
        assert!(!objects.discarding_oversized_forwarded_kitty_apc);
        assert!(objects.pending_bytes.len() <= 40);
    }

    #[test]
    fn bitmap_and_rgp_have_independent_framing_limits() {
        let mut objects = TerminalInlineObjects {
            rgp_apc_limit: Some(64),
            ..TerminalInlineObjects::default()
        };
        let mut parser = parser();
        let limit = 8;

        objects.consume_pty_output_with_bitmap_limit(b"\x1b_ratty;g;", &mut parser, limit);
        let replies = objects.consume_pty_output_with_bitmap_limit(b"s\x1b\\", &mut parser, limit);

        assert_eq!(replies, vec![crate::rgp::support_reply()]);
        assert!(contents(&parser).is_empty());
    }

    #[test]
    fn bounds_fragmented_rgp_apc_and_recovers_after_split_st() {
        let mut objects = TerminalInlineObjects {
            rgp_apc_limit: Some(40),
            ..TerminalInlineObjects::default()
        };
        let mut parser = parser();

        assert!(
            consume(
                &mut objects,
                b"before\x1b_ratty;g;r;id=1;fmt=obj;source=payload;AAAAAAAA",
                &mut parser,
            )
            .is_empty()
        );
        assert!(objects.pending_bytes.len() <= 40);
        assert!(objects.discarding_oversized_rgp_apc);
        assert_eq!(contents(&parser), "before");

        assert!(consume(&mut objects, b"discarded\x1b", &mut parser).is_empty());
        let replies = consume(&mut objects, b"\\after\x1b_ratty;g;s\x1b\\", &mut parser);
        assert_eq!(replies, vec![crate::rgp::support_reply()]);
        assert_eq!(contents(&parser), "beforeafter");
        assert!(!objects.discarding_oversized_rgp_apc);
        assert!(objects.pending_bytes.len() <= 40);
    }

    #[test]
    fn oversized_rgp_middle_chunk_aborts_pending_payload() {
        let mut objects = TerminalInlineObjects::default();
        let mut parser = parser();
        let first = base64::engine::general_purpose::STANDARD.encode(b"v 0 0 0\nv 1 ");
        let final_chunk =
            base64::engine::general_purpose::STANDARD.encode(b"0 0\nv 0 1 0\nf 1 2 3\n");
        consume(
            &mut objects,
            format!("\x1b_ratty;g;r;id=55;fmt=obj;source=payload;more=1;{first}\x1b\\").as_bytes(),
            &mut parser,
        );
        assert!(objects.pending_rgp_payloads.contains_key(&55));

        objects.rgp_apc_limit = Some(48);
        consume(
            &mut objects,
            format!(
                "\x1b_ratty;g;r;id=55;fmt=obj;source=payload;more=1;{}\x1b\\",
                "A".repeat(128)
            )
            .as_bytes(),
            &mut parser,
        );
        assert!(objects.pending_rgp_payloads.is_empty());

        objects.rgp_apc_limit = None;
        consume(
            &mut objects,
            format!("\x1b_ratty;g;r;id=55;fmt=obj;source=payload;more=0;{final_chunk}\x1b\\")
                .as_bytes(),
            &mut parser,
        );
        assert!(!objects.objects.contains_key(&55));
    }

    #[test]
    fn bounds_rgp_decoded_chunks_pending_bytes_and_transfer_count() {
        let mut objects = TerminalInlineObjects {
            rgp_max_payload_bytes: Some(8),
            rgp_max_pending_bytes: Some(6),
            rgp_max_pending_transfers: Some(2),
            ..TerminalInlineObjects::default()
        };
        let mut parser = parser();
        let chunk =
            |id| format!("\x1b_ratty;g;r;id={id};fmt=obj;source=payload;more=1;YWJjZA==\x1b\\");

        consume(&mut objects, chunk(1).as_bytes(), &mut parser);
        assert_eq!(objects.pending_rgp_payload_bytes(), 4);
        consume(&mut objects, chunk(2).as_bytes(), &mut parser);
        assert!(objects.pending_rgp_payloads.contains_key(&1));
        assert!(!objects.pending_rgp_payloads.contains_key(&2));
        assert_eq!(objects.pending_rgp_payload_bytes(), 4);

        objects.rgp_max_pending_bytes = Some(16);
        objects.rgp_max_pending_transfers = Some(1);
        consume(&mut objects, chunk(2).as_bytes(), &mut parser);
        assert!(!objects.pending_rgp_payloads.contains_key(&2));

        // One decoded chunk above the per-object cap is rejected before it
        // can create an incomplete transfer.
        objects.rgp_max_payload_bytes = Some(3);
        consume(&mut objects, chunk(3).as_bytes(), &mut parser);
        assert!(!objects.pending_rgp_payloads.contains_key(&3));
    }

    #[test]
    fn bounds_finalized_rgp_object_count_and_resident_bytes_with_reuse() {
        let triangle = b"v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n".to_vec();
        let mut objects = TerminalInlineObjects {
            rgp_max_payload_bytes: Some(4_096),
            rgp_max_total_bytes: Some(4_096),
            rgp_max_objects: Some(1),
            ..TerminalInlineObjects::default()
        };
        objects.handle_rgp_payload_chunk(
            1,
            "obj",
            None,
            false,
            triangle.clone(),
            ObjectLoadOptions::default(),
        );
        assert!(objects.objects.contains_key(&1));
        assert_eq!(objects.rgp_object_bytes.len(), 1);

        objects.handle_rgp_payload_chunk(
            2,
            "obj",
            None,
            false,
            triangle.clone(),
            ObjectLoadOptions::default(),
        );
        assert!(!objects.objects.contains_key(&2));

        // Reusing the existing ID replaces its budget entry rather than
        // consuming another object slot.
        objects.handle_rgp_payload_chunk(
            1,
            "obj",
            Some("replacement.obj".to_string()),
            false,
            triangle,
            ObjectLoadOptions { normalize: false },
        );
        assert_eq!(objects.objects.len(), 1);

        let resident = objects.rgp_object_bytes[&1];
        objects.rgp_max_objects = Some(2);
        objects.rgp_max_total_bytes = Some(resident);
        objects.handle_rgp_payload_chunk(
            2,
            "obj",
            None,
            false,
            b"v 0 0 0\nv 1 0 0\nv 0 1 0\nf 1 2 3\n".to_vec(),
            ObjectLoadOptions::default(),
        );
        assert!(!objects.objects.contains_key(&2));
    }

    #[test]
    fn rgp_glb_decodes_in_memory_and_replaces_without_runtime_asset() {
        let mut objects = TerminalInlineObjects {
            rgp_max_payload_bytes: Some(4_096),
            rgp_max_total_bytes: Some(4_096),
            rgp_max_objects: Some(2),
            ..TerminalInlineObjects::default()
        };
        let first_payload = minimal_rgp_glb(1);
        objects.handle_rgp_payload_chunk(
            7,
            "glb",
            None,
            false,
            first_payload,
            ObjectLoadOptions::default(),
        );
        match objects
            .objects
            .get(&7)
            .expect("first GLB payload should create an object")
        {
            InlineObject::RgpObject(RgpInlineObject::Obj { meshes, .. }) => {
                assert_eq!(meshes.len(), 1);
                assert_eq!(meshes[0].count_vertices(), 3);
            }
            _ => panic!("RGP GLB payload must decode into an owned mesh"),
        }

        objects.handle_rgp_payload_chunk(
            7,
            "glb",
            None,
            false,
            minimal_rgp_glb(2),
            ObjectLoadOptions::default(),
        );
        match objects
            .objects
            .get(&7)
            .expect("replacement GLB payload should create an object")
        {
            InlineObject::RgpObject(RgpInlineObject::Obj { meshes, .. }) => {
                assert_eq!(meshes.len(), 1);
                assert_eq!(meshes[0].count_vertices(), 3);
            }
            _ => panic!("replacement RGP GLB must remain an owned mesh"),
        }

        objects.remove_object(7);
        assert!(!objects.objects.contains_key(&7));
        assert!(!objects.rgp_object_bytes.contains_key(&7));
    }

    #[test]
    fn oversized_forwarded_kitty_chunk_aborts_rio_partial_transfer() {
        let mut objects = TerminalInlineObjects::default();
        let mut parser = parser();
        consume(
            &mut objects,
            b"\x1b_Ga=t,f=32,s=1,v=1,i=77,m=1;AAE=\x1b\\",
            &mut parser,
        );
        objects.forwarded_kitty_apc_limit = Some(48);
        let oversized = format!("\x1b_Gm=1;{}\x1b\\", "A".repeat(128));
        consume(&mut objects, oversized.as_bytes(), &mut parser);
        consume(&mut objects, b"\x1b_Gm=0;AgM=\x1b\\", &mut parser);

        assert!(parser.term.graphics.get_kitty_image(77).is_none());
    }

    #[test]
    fn stale_rgp_payload_transfer_expires_and_next_command_recovers() {
        let mut objects = TerminalInlineObjects {
            rgp_max_payload_bytes: Some(8),
            rgp_max_pending_bytes: Some(8),
            rgp_max_pending_transfers: Some(1),
            ..TerminalInlineObjects::default()
        };
        let mut parser = parser();
        consume(
            &mut objects,
            b"\x1b_ratty;g;r;id=1;fmt=obj;source=payload;more=1;YWJjZA==\x1b\\",
            &mut parser,
        );
        objects
            .pending_rgp_payloads
            .get_mut(&1)
            .expect("first RGP transfer should be pending")
            .last_touched = Instant::now() - RGP_INCOMPLETE_TIMEOUT - Duration::from_secs(1);

        let replies = consume(&mut objects, b"\x1b_ratty;g;s\x1b\\", &mut parser);
        assert!(objects.pending_rgp_payloads.is_empty());
        assert_eq!(replies, vec![crate::rgp::support_reply()]);
    }

    #[test]
    fn accepts_c1_st_for_bitmap_support_query() {
        let mut objects = TerminalInlineObjects::default();
        let mut parser = parser();

        let replies = consume(&mut objects, b"\x1b_ratty;i;s\x9c", &mut parser);

        assert_eq!(replies, vec![BITMAP_SUPPORT_REPLY.to_vec()]);
        assert!(contents(&parser).is_empty());
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
        assert!(contents(&parser).is_empty());
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
        assert!(contents(&parser).is_empty());
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
        assert_eq!(contents(&parser), "beforeafter");
    }

    #[test]
    fn bitmap_placements_keep_payload_state_static_while_terminal_geometry_scrolls() {
        let mut objects = TerminalInlineObjects::default();
        let mut parser = parser();
        register_bitmap(&mut objects, &mut parser);
        consume(
            &mut objects,
            b"\x1b_ratty;i;p;id=7;pid=9;row=5;col=2;w=8;h=3\x1b\\",
            &mut parser,
        );

        assert!(objects.needs_sync(Vec2::ZERO, 0, 0, &parser.term));
        objects.finish_sync(Vec2::ZERO, 0, 0, &parser.term);
        assert!(!objects.needs_sync(Vec2::ZERO, 0, 0, &parser.term));

        let mut scroll = scroll_one_row();
        scroll.extend(b"x\r\n");
        consume(&mut objects, &scroll, &mut parser);

        assert_eq!(
            objects
                .bitmap
                .placement(9)
                .expect("bitmap placement should exist")
                .row(),
            5,
            "Ratty keeps only immutable protocol payload/style state"
        );
        let geometry = parser
            .term
            .external_placement_geometries()
            .into_iter()
            .find(|geometry| geometry.id == bitmap_external_id(9))
            .expect("terminal should own the placement geometry");
        assert_eq!(geometry.row, 3);
        assert!(objects.needs_sync(Vec2::ZERO, 0, 0, &parser.term));
    }

    #[test]
    fn placement_before_scrolling_text_moves_in_wire_order() {
        let mut objects = TerminalInlineObjects::default();
        let mut parser = parser();
        register_bitmap(&mut objects, &mut parser);
        let mut chunk = b"\x1b_ratty;i;p;id=7;pid=9;row=5;col=2;w=8;h=3\x1b\\".to_vec();
        chunk.extend(scroll_one_row());

        consume(&mut objects, &chunk, &mut parser);

        assert_eq!(
            objects
                .bitmap
                .placement(9)
                .expect("placement should survive one row of scrolling")
                .row(),
            5
        );
        let geometry = parser
            .term
            .external_placement_geometries()
            .into_iter()
            .find(|geometry| geometry.id == bitmap_external_id(9))
            .expect("terminal should retain the placement");
        assert_eq!(geometry.row, 4);
    }

    #[test]
    fn placement_after_scrolling_text_is_not_moved_retroactively() {
        let mut objects = TerminalInlineObjects::default();
        let mut parser = parser();
        register_bitmap(&mut objects, &mut parser);
        consume(
            &mut objects,
            b"\x1b_ratty;i;p;id=7;pid=8;row=10;col=2;w=8;h=3\x1b\\",
            &mut parser,
        );
        let mut chunk = scroll_one_row();
        chunk.extend(b"\x1b_ratty;i;p;id=7;pid=9;row=5;col=2;w=8;h=3\x1b\\");

        consume(&mut objects, &chunk, &mut parser);

        let geometries = parser
            .term
            .external_placement_geometries()
            .into_iter()
            .map(|geometry| (geometry.id, geometry.row))
            .collect::<HashMap<_, _>>();
        assert_eq!(geometries[&bitmap_external_id(8)], 9);
        assert_eq!(geometries[&bitmap_external_id(9)], 5);
        assert_eq!(
            objects
                .bitmap
                .placement(8)
                .expect("payload state remains registered")
                .row(),
            10
        );
    }

    #[test]
    fn bitmap_and_rgp_geometry_follow_partial_margin_scroll_in_wire_order() {
        let mut objects = TerminalInlineObjects::default();
        let mut parser = TerminalRuntime::for_test(6, 20);
        register_bitmap(&mut objects, &mut parser);
        for object_id in [101, 102, 103, 104] {
            objects.objects.insert(
                object_id,
                InlineObject::RgpObject(RgpInlineObject::Gltf {
                    asset_path: format!("fixture-{object_id}.glb"),
                    handle: None,
                }),
            );
        }

        let wire = concat!(
            "\x1b_ratty;i;p;id=7;pid=11;row=1;col=2;w=2;h=2\x1b\\",
            "\x1b_ratty;i;p;id=7;pid=12;row=4;col=2;w=2;h=1\x1b\\",
            "\x1b_ratty;i;p;id=7;pid=13;row=0;col=2;w=2;h=2\x1b\\",
            "\x1b_ratty;g;p;id=101;row=2;col=3;w=2;h=2\x1b\\",
            "\x1b_ratty;g;p;id=102;row=4;col=3;w=2;h=1\x1b\\",
            "\x1b_ratty;g;p;id=103;row=1;col=3;w=2;h=2\x1b\\",
            "\x1b[2;4r\x1b[4;1H\n",
            "\x1b_ratty;i;p;id=7;pid=14;row=1;col=2;w=2;h=1\x1b\\",
            "\x1b_ratty;g;p;id=104;row=1;col=3;w=2;h=1\x1b\\",
        );
        assert!(consume(&mut objects, wire.as_bytes(), &mut parser).is_empty());

        let geometries = parser
            .term
            .external_placement_geometries()
            .into_iter()
            .map(|geometry| (geometry.id, geometry))
            .collect::<HashMap<_, _>>();
        for id in [bitmap_external_id(11), rgp_external_id(101)] {
            let geometry = geometries[&id];
            assert_eq!(
                (geometry.row, geometry.rows, geometry.source_row),
                (1, 1, 1)
            );
        }
        for id in [bitmap_external_id(12), rgp_external_id(102)] {
            let geometry = geometries[&id];
            assert_eq!(
                (geometry.row, geometry.rows, geometry.source_row),
                (4, 1, 0)
            );
        }
        for id in [bitmap_external_id(13), rgp_external_id(103)] {
            let geometry = geometries[&id];
            assert_eq!(
                (geometry.row, geometry.rows, geometry.source_row),
                (0, 2, 0)
            );
        }
        for id in [bitmap_external_id(14), rgp_external_id(104)] {
            let geometry = geometries[&id];
            assert_eq!(
                (geometry.row, geometry.rows, geometry.source_row),
                (1, 1, 0)
            );
        }

        consume(
            &mut objects,
            b"\x1b_ratty;i;u;pid=11;col=7\x1b\\",
            &mut parser,
        );
        let geometry = parser
            .term
            .external_placement_geometries()
            .into_iter()
            .find(|geometry| geometry.id == bitmap_external_id(11))
            .expect("column-only update must retain terminal placement state");
        assert_eq!(
            (
                geometry.row,
                geometry.col,
                geometry.rows,
                geometry.source_row
            ),
            (1, 7, 1, 1),
            "column-only updates must not revive rows clipped by a terminal mutation"
        );
    }

    #[test]
    fn wide_rgp_placement_centers_without_truncating_the_half_width() {
        let mut objects = TerminalInlineObjects::default();
        let mut parser = TerminalRuntime::for_test(6, 20);
        objects.objects.insert(
            77,
            InlineObject::RgpObject(RgpInlineObject::Gltf {
                asset_path: "fixture-77.glb".to_string(),
                handle: None,
            }),
        );

        assert!(
            consume(
                &mut objects,
                b"\x1b_ratty;g;p;id=77;row=1;col=10;w=131074;h=1\x1b\\",
                &mut parser,
            )
            .is_empty()
        );

        let placement = parser
            .term
            .external_placement(ExternalPlacementScreen::Main, rgp_external_id(77))
            .expect("wide RGP placement");
        assert_eq!(placement.col, 0);
        assert_eq!(placement.columns, 131_074);
    }

    #[test]
    fn bitmap_placements_are_screen_owned_and_pruned_after_alt_screen_reset() {
        let mut objects = TerminalInlineObjects::default();
        let mut parser = parser();
        register_bitmap(&mut objects, &mut parser);
        consume(
            &mut objects,
            b"\x1b_ratty;i;p;id=7;pid=21;row=2;col=2;w=2;h=1\x1b\\",
            &mut parser,
        );
        consume(&mut objects, b"\x1b[?1049h", &mut parser);
        assert!(parser.term.external_placement_geometries().is_empty());
        consume(
            &mut objects,
            b"\x1b_ratty;i;p;id=7;pid=22;row=3;col=2;w=2;h=1\x1b\\",
            &mut parser,
        );
        assert_eq!(
            parser.term.external_placement_geometries()[0].id,
            bitmap_external_id(22)
        );

        consume(&mut objects, b"\x1b[?1049l", &mut parser);
        assert_eq!(
            parser.term.external_placement_geometries()[0].id,
            bitmap_external_id(21)
        );
        consume(&mut objects, b"\x1b[?1049h", &mut parser);
        objects.reconcile_terminal_placements(&parser.term);
        assert!(parser.term.external_placement_geometries().is_empty());
        assert!(objects.bitmap.placement(22).is_none());
        assert!(objects.bitmap.placement(21).is_some());

        consume(&mut objects, b"\x1b[?1049l", &mut parser);
        assert_eq!(
            parser.term.external_placement_geometries()[0].id,
            bitmap_external_id(21)
        );
    }

    #[test]
    fn scrolled_bitmap_reappears_from_history_then_expires_with_the_ring() {
        let mut objects = TerminalInlineObjects::default();
        let mut parser = parser();
        register_bitmap(&mut objects, &mut parser);
        consume(
            &mut objects,
            b"\x1b_ratty;i;p;id=7;pid=31;row=0;col=2;w=2;h=1\x1b\\",
            &mut parser,
        );
        consume(&mut objects, &scroll_one_row(), &mut parser);
        assert_eq!(parser.term.external_placement_geometries()[0].row, -1);

        vt::set_scrollback(&mut parser.term, 1);
        assert_eq!(parser.term.external_placement_geometries()[0].row, 0);
        vt::set_scrollback(&mut parser.term, 0);

        consume(&mut objects, &b"x\r\n".repeat(1_100), &mut parser);
        objects.reconcile_terminal_placements(&parser.term);
        assert!(
            parser
                .term
                .external_placement(ExternalPlacementScreen::Main, bitmap_external_id(31))
                .is_none()
        );
        assert!(objects.bitmap.placement(31).is_none());
    }
}
