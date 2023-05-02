use bevy::window::PrimaryWindow;
use bevy_matchbox::prelude::PeerId;
use ggrs::Config;

use crate::prelude::*;

// These are just 16 bit for bit-packing alignment in the input struct
const INPUT_UP: u16 = 0b00001;
const INPUT_DOWN: u16 = 0b00010;
const INPUT_LEFT: u16 = 0b00100;
const INPUT_RIGHT: u16 = 0b01000;

/// GGRS player handle, we use this to associate GGRS handles back to our [`Entity`]
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default, Component)]
pub struct Player {
    pub handle: usize,
}

/// Local handles, this should just be 1 entry in this demo, but you may end up wanting to implement 2v2
#[derive(Default, Resource)]
pub struct LocalHandles {
    pub handles: Vec<PlayerHandle>,
}

/// The main GGRS configuration type
#[derive(Debug)]
pub struct GGRSConfig;
impl Config for GGRSConfig {
    type Input = GGRSInput;
    // bevy_ggrs doesn't really use State, so just make this a small whatever
    type State = u8;
    type Address = PeerId;
}

/// Our primary data struct; what players send to one another
#[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, Pod, Zeroable)]
pub struct GGRSInput {
    // The input from our player
    pub input: u16,
    _padding1: u16, // Keep things 32-bit-aligned for Bytemuck

    pub mouse_visible: u8,
    pub mouse_clicked: u8,
    _padding2: u16, // Keep things 32-bit-aligned for Bytemuck

    pub mouse_x: i32,
    pub mouse_y: i32,

    // Desync detection
    pub last_confirmed_hash: u16,
    _padding3: u16, // Keep things 32-bit-aligned for Bytemuck

    pub last_confirmed_frame: Frame,
    // Ok, so I know what you're thinking:
    // > "That's not input!"
    // Well, you're right, and we're going to abuse the existing socket to also
    // communicate about the last confirmed frame we saw and what was the hash
    // of the physics state.  This allows us to detect desync.  This could also
    // use a new socket, but who wants to hole punch twice?  I have been working
    // on a GGRS branch (linked below) that introduces a new message type, but
    // it is not ready.  However, input-packing works good enough for now.
    // https://github.com/cscorley/ggrs/tree/arbitrary-messages-0.8
}

pub fn input(
    handle: In<PlayerHandle>,
    local_handles: Res<LocalHandles>,
    keyboard_input: Res<Input<KeyCode>>,
    mut random: ResMut<RandomInput>,
    physics_enabled: Res<PhysicsEnabled>,
    mut hashes: ResMut<FrameHashes>,
    validatable_frame: Res<ValidatableFrame>,
    window: Query<&Window, With<PrimaryWindow>>,
    camera: Query<(&Camera, &GlobalTransform)>,
    mouse_buttons: Res<Input<MouseButton>>,
) -> GGRSInput {
    let mut input = GGRSInput {
        last_confirmed_frame: ggrs::NULL_FRAME,
        ..default()
    };

    // Find a hash that we haven't sent yet.
    // This probably seems like overkill but we have to track a bunch anyway, we
    // might as well do our due diligence and inform our opponent of every hash
    // we have This may mean we ship them out of order.  The important thing is
    // we determine the desync *eventually* because that match is pretty much
    // invalidated without a state synchronization mechanism (which GGRS/GGPO
    // does not have out of the box.)
    for frame_hash in hashes.0.iter_mut() {
        // only send confirmed frames that have not yet been sent that are well past our max prediction window
        if frame_hash.confirmed
            && !frame_hash.sent
            && validatable_frame.is_validatable(frame_hash.frame)
        {
            info!("Sending data {:?}", frame_hash);
            input.last_confirmed_frame = frame_hash.frame;
            input.last_confirmed_hash = frame_hash.rapier_checksum;
            frame_hash.sent = true;
        }
    }

    // Do not do anything until physics are live
    if !physics_enabled.0 {
        return input;
    }

    // Build the input
    if keyboard_input.pressed(KeyCode::W) {
        input.input |= INPUT_UP;
    }
    if keyboard_input.pressed(KeyCode::A) {
        input.input |= INPUT_LEFT;
    }
    if keyboard_input.pressed(KeyCode::S) {
        input.input |= INPUT_DOWN;
    }
    if keyboard_input.pressed(KeyCode::D) {
        input.input |= INPUT_RIGHT;
    }

    // toggle off random input if our local moves at all
    if input.input != 0 && random.on && local_handles.handles.contains(&handle.0) {
        random.on = false;
    } else if input.input == 0 && random.on && local_handles.handles.contains(&handle.0) {
        let mut rng = thread_rng();
        // Return a random input sometimes, or maybe nothing.
        // Helps to trigger input-based rollbacks from the unplayed side
        match rng.gen_range(0..10) {
            0 => input.input = INPUT_UP,
            1 => input.input = INPUT_LEFT,
            2 => input.input = INPUT_DOWN,
            3 => input.input = INPUT_RIGHT,
            _ => (),
        }
    }

    // handle mouse input, if any
    let (camera, camera_transform) = camera.single();
    if let Some(pos) = window
        .single()
        .cursor_position()
        .and_then(|cursor| camera.viewport_to_world(camera_transform, cursor))
        .map(|ray| ray.origin.truncate())
    {
        input.mouse_visible = 1;
        input.mouse_clicked = if mouse_buttons.just_released(MouseButton::Left) {
            1
        } else {
            0
        };
        input.mouse_x = pos.x as i32;
        input.mouse_y = pos.y as i32;
    }

    input
}

pub fn apply_inputs(
    mut query: Query<(&mut Velocity, &Player)>,
    inputs: Res<PlayerInputs<GGRSConfig>>,
    mut hashes: ResMut<RxFrameHashes>,
    local_handles: Res<LocalHandles>,
    physics_enabled: Res<PhysicsEnabled>,
) {
    for (mut v, p) in query.iter_mut() {
        let (game_input, input_status) = inputs[p.handle];
        // Check the desync for this player if they're not a local handle
        // Did they send us some goodies?
        if !local_handles.handles.contains(&p.handle) && game_input.last_confirmed_frame > 0 {
            log::info!("Got frame data {:?}", game_input);
            if let Some(frame_hash) = hashes
                .0
                .get_mut((game_input.last_confirmed_frame as usize) % DESYNC_MAX_FRAMES)
            {
                assert!(
                    frame_hash.frame != game_input.last_confirmed_frame
                        || frame_hash.rapier_checksum == game_input.last_confirmed_hash,
                    "Got new data for existing frame data {}",
                    frame_hash.frame
                );

                // Only update this local data if the frame is new-to-us.
                // We don't want to overwrite any existing validated status
                // unless the frame is replacing what is already in the buffer.
                if frame_hash.frame != game_input.last_confirmed_frame {
                    frame_hash.frame = game_input.last_confirmed_frame;
                    frame_hash.rapier_checksum = game_input.last_confirmed_hash;
                    frame_hash.validated = false;
                }
            }
        }

        // On to the boring stuff
        let input = match input_status {
            InputStatus::Confirmed => game_input.input,
            InputStatus::Predicted => game_input.input,
            InputStatus::Disconnected => 0, // disconnected players do nothing
        };

        if input > 0 {
            // Useful for desync observing
            log::info!("input {:?} from {}: {}", input_status, p.handle, input)
        }

        // Do not do anything until physics are live
        // This is a poor mans emulation to stop accidentally tripping velocity updates
        if !physics_enabled.0 {
            continue;
        }

        let right = input & INPUT_RIGHT != 0;
        let left = input & INPUT_LEFT != 0;
        let up = input & INPUT_UP != 0;
        let down = input & INPUT_DOWN != 0;

        let direction_right = right && !left;
        let direction_left = left && !right;
        let direction_up = up && !down;
        let direction_down = down && !up;

        let horizontal = if direction_left {
            -1.
        } else if direction_right {
            1.
        } else {
            0.
        };

        let vertical = if direction_down {
            -1.
        } else if direction_up {
            1.
        } else {
            0.
        };

        let new_vel_x = if horizontal != 0. {
            v.linvel.x + horizontal * 10.0
        } else {
            0.
        };

        let new_vel_y = if vertical != 0. {
            v.linvel.y + vertical * 10.0
        } else {
            0.
        };

        // This is annoying but we have to make sure we only trigger an update in Rapier when explicitly necessary!
        if new_vel_x != v.linvel.x || new_vel_y != v.linvel.y {
            v.linvel.x = new_vel_x;
            v.linvel.y = new_vel_y;
        }

        // handle mouse, if any
        if game_input.mouse_visible == 1 {
            log::info!("mouse visible! {:?}", game_input);
        }
        if game_input.mouse_clicked == 1 {
            log::info!("mouse clicked! {:?}", game_input);
        }
    }
}

pub fn force_update_rollbackables(
    mut t_query: Query<&mut Transform, With<Rollback>>,
    mut v_query: Query<&mut Velocity, With<Rollback>>,
) {
    for mut t in t_query.iter_mut() {
        t.set_changed();
    }
    for mut v in v_query.iter_mut() {
        v.set_changed();
    }
}
