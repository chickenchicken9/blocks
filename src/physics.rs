use crate::prelude::*;

/// Our physics rollback state container, which will be rolled back and we will
/// use to restore our physics state.
#[derive(Default, Reflect, Hash, Resource, PartialEq, Eq)]
#[reflect(Hash, Resource, PartialEq)]
pub struct PhysicsRollbackState {
    pub rapier_state: Option<Vec<u8>>,
    pub rapier_checksum: u16,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Default, Resource, Hash, Reflect)]
#[reflect(Hash)]
pub struct PhysicsEnabled(pub bool);

#[derive(Copy, Clone, PartialEq, Eq, Debug, Resource, Hash, Reflect)]
#[reflect(Hash, Resource, PartialEq)]
pub struct EnablePhysicsAfter {
    pub start: Frame,
    pub end: Frame,
}

impl Default for EnablePhysicsAfter {
    fn default() -> Self {
        Self::with_default_offset(0)
    }
}

impl EnablePhysicsAfter {
    pub fn new(start: Frame, end: Frame) -> Self {
        log::info!("Enabling after {:?},{:?}", start, end);
        Self { start, end }
    }

    pub fn with_default_offset(offset: Frame) -> Self {
        Self::new(offset, offset + (FPS * LOAD_SECONDS) as i32)
    }

    pub fn is_enabled(&self, frame: Frame) -> bool {
        // Since the starting frame is calculated at the end,
        // when we rollback to the start frame we will have the enable after
        // resource of that frame it was created as a result of, which is wrong.
        // assume that 1 frame is actually good and should not be ignored
        !(self.start < frame && frame < self.end)
    }
}

pub fn toggle_physics(
    enable_physics_after: Res<EnablePhysicsAfter>,
    current_frame: Res<CurrentFrame>,
    mut physics_enabled: ResMut<PhysicsEnabled>,
    mut config: ResMut<RapierConfiguration>,
) {
    log::info!(
        "Physics on frame {:?} {:?} {:?}",
        current_frame.0,
        physics_enabled.0,
        enable_physics_after
    );
    let should_activate = enable_physics_after.is_enabled(current_frame.0);
    if physics_enabled.0 != should_activate {
        log::info!(
            "Toggling physics on frame {:?}: {:?} -> {:?}",
            current_frame.0,
            physics_enabled.0,
            should_activate
        );
        physics_enabled.0 = should_activate;
    }

    config.physics_pipeline_active = physics_enabled.0;
}

pub fn rollback_rapier_context(
    rollback_status: Res<RollbackStatus>,
    game_state: Res<PhysicsRollbackState>,
    mut rapier: ResMut<RapierContext>,
) {
    let mut checksum = game_state.rapier_checksum;
    log::info!("Context pre-hash at start: {:?}", checksum);

    // Serialize our physics state for hashing, to display the state in-flight.
    // This should not be necessary for this demo to work, as we will do the
    // real checksum during `save_game_state` at the end of the pipeline.
    if let Ok(context_bytes) = bincode::serialize(rapier.as_ref()) {
        checksum = fletcher16(&context_bytes);
        log::info!("Context hash at start: {}", checksum);
    }

    // Only restore our state if we are in a rollback.  This step is *critical*.
    // Only doing this during rollbacks saves us a step every frame.  Here, we
    // also do not allow rollback to frame 0.  Physics state is already correct
    // in this case.  This prevents lagged clients from getting immediate desync
    // and is entirely a hack since we don't enable physics until later anyway.
    //
    // You can also test that desync detection is working by disabling:
    // if false {
    if rollback_status.is_rollback && rollback_status.rollback_frame > 1 {
        if let Some(state_context) = game_state.rapier_state.as_ref() {
            if let Ok(context) = bincode::deserialize::<RapierContext>(state_context) {
                // commands.insert_resource(context);
                // *rapier = context;

                // Inserting or replacing directly seems to screw up some of the
                // crate-only properties.  So, we'll copy over each public
                // property instead.
                rapier.bodies = context.bodies;
                rapier.broad_phase = context.broad_phase;
                rapier.ccd_solver = context.ccd_solver;
                rapier.colliders = context.colliders;
                rapier.impulse_joints = context.impulse_joints;
                rapier.integration_parameters = context.integration_parameters;
                rapier.islands = context.islands;
                rapier.multibody_joints = context.multibody_joints;
                rapier.narrow_phase = context.narrow_phase;
                rapier.query_pipeline = context.query_pipeline;

                // pipeline is not serialized
                // rapier.pipeline = context.pipeline;
            }
        }

        // Again, not necessary for the demo, just to show the rollback changes
        // as they occur.
        if let Ok(context_bytes) = bincode::serialize(rapier.as_ref()) {
            log::info!(
                "Context hash after rollback: {}",
                fletcher16(&context_bytes)
            );
        }
    }
}

pub fn save_rapier_context(
    mut game_state: ResMut<PhysicsRollbackState>,
    rapier: Res<RapierContext>,
    mut hashes: ResMut<FrameHashes>,
    confirmed_frame: Res<ConfirmedFrame>,
    current_frame: Res<CurrentFrame>,
) {
    // This serializes our context every frame.  It's not great, but works to
    // integrate the two plugins.  To do less of it, we would need to change
    // bevy_ggrs to serialize arbitrary structs like this one in addition to
    // component tracking.  If you need this to happen less, I'd recommend not
    // using the plugin and implementing GGRS yourself.
    if let Ok(context_bytes) = bincode::serialize(rapier.as_ref()) {
        log::info!("Context hash before save: {}", game_state.rapier_checksum);
        game_state.rapier_checksum = fletcher16(&context_bytes);
        game_state.rapier_state = Some(context_bytes);
        log::info!("Context hash after save: {}", game_state.rapier_checksum);

        if let Some(frame_hash) = hashes
            .0
            .get_mut((current_frame.0 as usize) % DESYNC_MAX_FRAMES)
        {
            if frame_hash.frame == current_frame.0 && frame_hash.sent {
                // If this frame hash has already been sent and its the
                // same one then the hashes better damn well match
                assert_eq!(
                    frame_hash.rapier_checksum, game_state.rapier_checksum,
                    "INTEGRITY BREACHED"
                );
                log::info!(
                    "Integrity challenged of frame {}: {} vs {}",
                    frame_hash.frame,
                    frame_hash.rapier_checksum,
                    game_state.rapier_checksum
                );
            }

            frame_hash.frame = current_frame.0;
            frame_hash.rapier_checksum = game_state.rapier_checksum;
            frame_hash.sent = false;
            frame_hash.validated = false;
            log::debug!("confirmed frame: {:?}", confirmed_frame);
            frame_hash.confirmed = frame_hash.frame <= confirmed_frame.0;
            log::debug!("Stored frame hash at save: {:?}", frame_hash);
        }

        log::info!("----- end frame {} -----", current_frame.0);
    }
}
