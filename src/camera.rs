//! Terminal camera presets, protocol updates, and activation systems.

use bevy::prelude::*;

use crate::scene::{MobiusTransition, TerminalPresentationMode};

/// Number of addressable terminal camera presets.
pub const TERMINAL_CAMERA_SLOT_COUNT: usize = 10;
/// Smallest valid perspective field of view in radians.
pub const MIN_PERSPECTIVE_FOV: f32 = 0.05;
/// Largest valid perspective field of view in radians.
pub const MAX_PERSPECTIVE_FOV: f32 = std::f32::consts::PI - 0.05;
/// Smallest valid orthographic projection scale.
pub const MIN_ORTHOGRAPHIC_SCALE: f32 = 0.01;

/// Ordered camera pipeline stages with actual resource dependencies.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub enum TerminalCameraSystemSet {
    /// Apply parsed RGP partial updates.
    ProtocolUpdates,
    /// Apply keyboard mode changes and queue slot activations.
    KeyboardInput,
    /// Activate presets after their updates and keyboard requests.
    Activation,
    /// Apply mouse interaction to the active pose.
    MouseInput,
    /// Advance explicit Mobius transitions.
    Transition,
    /// Synchronize the resulting state to Bevy entities.
    Synchronize,
}

/// Persistent pose for a terminal camera preset.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TerminalCameraPose {
    /// Rotation around the vertical axis, in radians.
    pub yaw: f32,
    /// Rotation around the horizontal axis, in radians.
    pub pitch: f32,
    /// Rotation around the view axis, in radians.
    pub roll: f32,
    /// Orthographic projection scale.
    pub orthographic_scale: f32,
    /// Perspective vertical field of view in radians.
    pub perspective_fov: f32,
    /// Camera translation relative to its default position.
    pub translation: Vec3,
}

impl TerminalCameraPose {
    /// Returns a finite, positive orthographic scale.
    pub fn resolved_orthographic_scale(self) -> f32 {
        if self.orthographic_scale.is_finite() {
            self.orthographic_scale.max(MIN_ORTHOGRAPHIC_SCALE)
        } else {
            1.0
        }
    }

    /// Returns a finite perspective field of view away from zero and pi.
    pub fn resolved_perspective_fov(self) -> f32 {
        if self.perspective_fov.is_finite() {
            self.perspective_fov
                .clamp(MIN_PERSPECTIVE_FOV, MAX_PERSPECTIVE_FOV)
        } else {
            1.0
        }
    }
}

impl Default for TerminalCameraPose {
    fn default() -> Self {
        Self {
            yaw: 0.18,
            pitch: 0.08,
            roll: 0.0,
            orthographic_scale: 1.0,
            perspective_fov: 1.0,
            translation: Vec3::ZERO,
        }
    }
}

/// Presentation mode and pose stored in one camera slot.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct TerminalCameraPreset {
    /// Presentation mode selected by the preset.
    pub mode: TerminalPresentationMode,
    /// Persistent camera pose selected by the preset.
    pub pose: TerminalCameraPose,
}

/// The ten persistent camera presets and the active slot.
#[derive(Resource, Clone, Debug)]
pub struct TerminalCameraSlots {
    presets: [TerminalCameraPreset; TERMINAL_CAMERA_SLOT_COUNT],
    active_slot: usize,
}

impl TerminalCameraSlots {
    /// Returns the active slot index.
    pub fn active_slot(&self) -> usize {
        self.active_slot
    }

    /// Returns the active preset.
    pub fn active(&self) -> &TerminalCameraPreset {
        &self.presets[self.active_slot]
    }

    /// Returns the active preset for a user interaction update.
    pub fn active_mut(&mut self) -> &mut TerminalCameraPreset {
        &mut self.presets[self.active_slot]
    }

    /// Returns a preset by slot index.
    pub fn preset(&self, slot: usize) -> Option<&TerminalCameraPreset> {
        self.presets.get(slot)
    }

    /// Activates a preset, returning whether the active slot changed.
    pub fn activate(&mut self, slot: usize) -> bool {
        if slot >= TERMINAL_CAMERA_SLOT_COUNT || self.active_slot == slot {
            return false;
        }
        self.active_slot = slot;
        true
    }

    fn updated_preset(&self, update: TerminalCameraUpdate) -> Option<TerminalCameraPreset> {
        let current = self.presets.get(update.slot).copied()?;
        let mut next = current;

        if let Some(mode) = update.mode {
            next.mode = mode;
        }
        if let Some(value) = update.translation.x {
            next.pose.translation.x = value;
        }
        if let Some(value) = update.translation.y {
            next.pose.translation.y = value;
        }
        if let Some(value) = update.translation.z {
            next.pose.translation.z = value;
        }
        if let Some(value) = update.rotation_degrees.x {
            next.pose.pitch = value.to_radians();
        }
        if let Some(value) = update.rotation_degrees.y {
            next.pose.yaw = value.to_radians();
        }
        if let Some(value) = update.rotation_degrees.z {
            next.pose.roll = value.to_radians();
        }
        if let Some(value) = update.scale {
            match next.mode {
                TerminalPresentationMode::Flat2d => {}
                TerminalPresentationMode::Plane3d | TerminalPresentationMode::Mobius3d => {
                    if value >= MIN_ORTHOGRAPHIC_SCALE {
                        next.pose.orthographic_scale = value;
                    }
                }
                TerminalPresentationMode::Perspective3d => {
                    if (MIN_PERSPECTIVE_FOV..=MAX_PERSPECTIVE_FOV).contains(&value) {
                        next.pose.perspective_fov = value;
                    }
                }
            }
        }

        if next == current {
            return None;
        }
        Some(next)
    }

    #[cfg(test)]
    fn apply_update(&mut self, update: TerminalCameraUpdate) -> bool {
        let Some(next) = self.updated_preset(update) else {
            return false;
        };
        self.presets[update.slot] = next;
        true
    }
}

impl Default for TerminalCameraSlots {
    fn default() -> Self {
        Self {
            presets: [TerminalCameraPreset::default(); TERMINAL_CAMERA_SLOT_COUNT],
            active_slot: 0,
        }
    }
}

/// Transient mouse interaction state, kept outside persistent presets.
#[derive(Resource, Default)]
pub struct TerminalCameraInteraction {
    /// Indicates an active rotation drag.
    pub rotating: bool,
    /// Indicates an active panning drag.
    pub panning: bool,
    /// Previous cursor position during rotation.
    pub last_rotate_cursor: Option<Vec2>,
    /// Previous cursor position during panning.
    pub last_pan_cursor: Option<Vec2>,
}

impl TerminalCameraInteraction {
    /// Clears transient drag state after a mode or slot transition.
    pub fn reset(&mut self) {
        self.rotating = false;
        self.panning = false;
        self.last_rotate_cursor = None;
        self.last_pan_cursor = None;
    }
}

/// Optional vector components in a partial camera update.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct OptionalVec3 {
    /// Optional X component.
    pub x: Option<f32>,
    /// Optional Y component.
    pub y: Option<f32>,
    /// Optional Z component.
    pub z: Option<f32>,
}

impl From<[Option<f32>; 3]> for OptionalVec3 {
    fn from(value: [Option<f32>; 3]) -> Self {
        Self {
            x: value[0],
            y: value[1],
            z: value[2],
        }
    }
}

/// Parsed partial camera update queued by the RGP input path.
#[derive(Message, Clone, Copy, Debug, PartialEq)]
pub struct TerminalCameraUpdate {
    /// Slot to update.
    pub slot: usize,
    /// Whether to activate the slot after applying the update.
    pub activate: bool,
    /// Optional presentation mode.
    pub mode: Option<TerminalPresentationMode>,
    /// Optional orthographic scale or perspective FOV in radians.
    pub scale: Option<f32>,
    /// Optional translation components.
    pub translation: OptionalVec3,
    /// Optional rotation components in degrees, mapped as pitch, yaw, and roll.
    pub rotation_degrees: OptionalVec3,
}

/// Request to activate one of the ten camera presets.
#[derive(Message, Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActivateTerminalCameraPreset {
    /// Slot to activate.
    pub slot: usize,
}

/// Applies queued RGP partial updates before any requested activation.
pub fn apply_terminal_camera_updates(
    mut updates: MessageReader<TerminalCameraUpdate>,
    mut activations: MessageWriter<ActivateTerminalCameraPreset>,
    mut slots: ResMut<TerminalCameraSlots>,
    mut interaction: ResMut<TerminalCameraInteraction>,
    mut mobius_transition: ResMut<MobiusTransition>,
) {
    for update in updates.read().copied() {
        let previous = slots.preset(update.slot).copied();
        let changed = if let Some(next) = slots.updated_preset(update) {
            slots.presets[update.slot] = next;
            true
        } else {
            false
        };
        if changed && let Some(previous) = previous {
            let next = slots.presets[update.slot];
            let mode_changed = next.mode != previous.mode;
            if update.slot == slots.active_slot() || update.activate {
                if next.mode == TerminalPresentationMode::Mobius3d {
                    let source_mode = if mode_changed {
                        previous.mode
                    } else if mobius_transition.source_is_for(update.slot) {
                        mobius_transition.source_mode
                    } else {
                        TerminalPresentationMode::Plane3d
                    };
                    mobius_transition.prepare_source(update.slot, source_mode, &next.pose);
                } else if mode_changed && update.slot == slots.active_slot() {
                    mobius_transition.stop();
                }
            }
            if mode_changed && update.slot == slots.active_slot() {
                interaction.reset();
            }
        }
        if update.activate {
            activations.write(ActivateTerminalCameraPreset { slot: update.slot });
        }
    }
}

/// Activates requested presets after their partial RGP updates have been applied.
pub fn activate_terminal_camera_presets(
    mut activations: MessageReader<ActivateTerminalCameraPreset>,
    mut slots: ResMut<TerminalCameraSlots>,
    mut interaction: ResMut<TerminalCameraInteraction>,
    mut mobius_transition: ResMut<MobiusTransition>,
) {
    for activation in activations.read().copied() {
        let should_activate =
            activation.slot < TERMINAL_CAMERA_SLOT_COUNT && slots.active_slot() != activation.slot;
        if should_activate {
            slots.activate(activation.slot);
            interaction.reset();
            mobius_transition.stop();
            if slots.active().mode == TerminalPresentationMode::Mobius3d {
                let source_mode = if mobius_transition.source_is_for(activation.slot) {
                    mobius_transition.source_mode
                } else {
                    TerminalPresentationMode::Plane3d
                };
                mobius_transition.prepare_source(
                    activation.slot,
                    source_mode,
                    &slots.active().pose,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::TerminalRedrawState;

    fn update(slot: usize) -> TerminalCameraUpdate {
        TerminalCameraUpdate {
            slot,
            activate: false,
            mode: None,
            scale: None,
            translation: OptionalVec3::default(),
            rotation_degrees: OptionalVec3::default(),
        }
    }

    #[test]
    fn partial_updates_preserve_omitted_pose_fields() {
        let mut slots = TerminalCameraSlots::default();
        let original = *slots.preset(3).expect("preset");
        let mut partial = update(3);
        partial.translation.x = Some(12.0);
        partial.rotation_degrees.y = Some(90.0);

        assert!(slots.apply_update(partial));
        let preset = slots.preset(3).expect("preset");
        assert_eq!(preset.mode, original.mode);
        assert_eq!(preset.pose.translation, Vec3::new(12.0, 0.0, 0.0));
        assert_eq!(preset.pose.yaw, std::f32::consts::FRAC_PI_2);
        assert_eq!(preset.pose.pitch, original.pose.pitch);
        assert_eq!(preset.pose.roll, original.pose.roll);
        assert_eq!(
            preset.pose.orthographic_scale,
            original.pose.orthographic_scale
        );
        assert_eq!(preset.pose.perspective_fov, original.pose.perspective_fov);
    }

    #[test]
    fn protocol_axes_map_to_pitch_yaw_and_roll_in_degrees() {
        let mut slots = TerminalCameraSlots::default();
        let mut partial = update(0);
        partial.rotation_degrees = OptionalVec3 {
            x: Some(10.0),
            y: Some(20.0),
            z: Some(30.0),
        };

        assert!(slots.apply_update(partial));
        let pose = slots.active().pose;
        assert_eq!(pose.pitch, 10.0_f32.to_radians());
        assert_eq!(pose.yaw, 20.0_f32.to_radians());
        assert_eq!(pose.roll, 30.0_f32.to_radians());
    }

    #[test]
    fn invalid_projection_scales_do_not_replace_the_previous_value() {
        let mut slots = TerminalCameraSlots::default();
        let mut partial = update(0);
        partial.mode = Some(TerminalPresentationMode::Plane3d);
        partial.scale = Some(MIN_ORTHOGRAPHIC_SCALE / 2.0);

        assert!(slots.apply_update(partial));
        assert_eq!(slots.active().mode, TerminalPresentationMode::Plane3d);
        assert_eq!(slots.active().pose.orthographic_scale, 1.0);

        let mut partial = update(0);
        partial.mode = Some(TerminalPresentationMode::Perspective3d);
        partial.scale = Some(std::f32::consts::PI);

        assert!(slots.apply_update(partial));
        assert_eq!(slots.active().mode, TerminalPresentationMode::Perspective3d);
        assert_eq!(slots.active().pose.perspective_fov, 1.0);
    }

    #[test]
    fn activation_rejects_out_of_range_slots() {
        let mut slots = TerminalCameraSlots::default();
        assert!(!slots.activate(TERMINAL_CAMERA_SLOT_COUNT));
        assert_eq!(slots.active_slot(), 0);
    }

    #[test]
    fn projections_are_always_finite_and_valid() {
        let mut pose = TerminalCameraPose::default();
        for value in [f32::NAN, f32::INFINITY, -1.0, 0.0, std::f32::consts::PI] {
            pose.orthographic_scale = value;
            pose.perspective_fov = value;
            assert!(pose.resolved_orthographic_scale().is_finite());
            assert!(pose.resolved_orthographic_scale() > 0.0);
            assert!(pose.resolved_perspective_fov().is_finite());
            assert!(pose.resolved_perspective_fov() > 0.0);
            assert!(pose.resolved_perspective_fov() < std::f32::consts::PI);
        }
    }

    #[test]
    fn orthographic_zoom_does_not_change_perspective_fov() {
        let mut slots = TerminalCameraSlots::default();
        slots.active_mut().mode = TerminalPresentationMode::Plane3d;
        slots.active_mut().pose.orthographic_scale = 4.0;
        let expected_fov = slots.active().pose.perspective_fov;

        slots.active_mut().mode = TerminalPresentationMode::Perspective3d;

        assert_eq!(slots.active().pose.perspective_fov, expected_fov);
        assert_eq!(slots.active().pose.resolved_perspective_fov(), 1.0);
    }

    #[test]
    fn immediate_activation_happens_after_the_slot_update() {
        let mut app = App::new();
        app.init_resource::<TerminalCameraSlots>()
            .init_resource::<TerminalCameraInteraction>()
            .init_resource::<MobiusTransition>()
            .add_message::<TerminalCameraUpdate>()
            .add_message::<ActivateTerminalCameraPreset>()
            .add_systems(
                Update,
                (
                    apply_terminal_camera_updates,
                    activate_terminal_camera_presets,
                )
                    .chain(),
            );
        let mut command = update(4);
        command.activate = true;
        command.mode = Some(TerminalPresentationMode::Perspective3d);
        command.translation.z = Some(50.0);
        app.world_mut().write_message(command);

        app.update();

        let slots = app.world().resource::<TerminalCameraSlots>();
        assert_eq!(slots.active_slot(), 4);
        assert_eq!(slots.active().mode, TerminalPresentationMode::Perspective3d);
        assert_eq!(slots.active().pose.translation.z, 50.0);
    }

    #[test]
    fn application_updates_remain_after_user_interaction_and_slot_switches() {
        let mut slots = TerminalCameraSlots::default();
        let mut command = update(2);
        command.mode = Some(TerminalPresentationMode::Plane3d);
        command.translation.x = Some(25.0);
        slots.apply_update(command);
        slots.activate(2);

        slots.active_mut().pose.yaw += 0.5;
        let expected = *slots.active();
        slots.activate(1);
        slots.activate(2);

        assert_eq!(*slots.active(), expected);
    }

    #[test]
    fn camera_protocol_updates_do_not_request_terminal_texture_redraws() {
        let mut app = App::new();
        app.init_resource::<TerminalCameraSlots>()
            .init_resource::<TerminalCameraInteraction>()
            .init_resource::<MobiusTransition>()
            .init_resource::<TerminalRedrawState>()
            .add_message::<TerminalCameraUpdate>()
            .add_message::<ActivateTerminalCameraPreset>()
            .add_systems(
                Update,
                (
                    apply_terminal_camera_updates,
                    activate_terminal_camera_presets,
                )
                    .chain(),
            );
        assert!(app.world_mut().resource_mut::<TerminalRedrawState>().take());
        {
            let mut interaction = app.world_mut().resource_mut::<TerminalCameraInteraction>();
            interaction.rotating = true;
            interaction.last_rotate_cursor = Some(Vec2::new(10.0, 20.0));
        }
        let mut command = update(0);
        command.mode = Some(TerminalPresentationMode::Perspective3d);
        app.world_mut().write_message(command);

        app.update();

        assert!(!app.world_mut().resource_mut::<TerminalRedrawState>().take());
        let interaction = app.world().resource::<TerminalCameraInteraction>();
        assert!(!interaction.rotating);
        assert_eq!(interaction.last_rotate_cursor, None);
    }
}
