use crate::game_info::{FrameInfo, GameInput};
use crate::network_stats::NetworkStats;
use crate::player::Player;
use crate::sync_layer::SyncLayer;
use crate::{circular_buffer::CircularBuffer, NULL_FRAME};
use crate::{FrameNumber, GGEZError, GGEZInterface, GGEZSession, PlayerHandle};

/// During a SyncTestSession, GGEZ will simulate a rollback every frame and resimulate the last n states, where n is the given check distance. If you provide checksums
/// in your [GGEZInterface::save_game_state()] function, the SyncTestSession will compare the resimulated checksums with the original checksums and report if there was a mismatch.
#[derive(Debug)]
pub struct SyncTestSession {
    current_frame: FrameNumber,
    num_players: u32,
    input_size: usize,
    check_distance: u32,
    running: bool,
    current_input: GameInput,
    saved_frames: CircularBuffer<FrameInfo>,
    sync_layer: SyncLayer,
}

impl SyncTestSession {
    /// Creates a new [SyncTestSession] instance with given values.
    pub fn new(check_distance: u32, num_players: u32, input_size: usize) -> SyncTestSession {
        SyncTestSession {
            current_frame: NULL_FRAME,
            num_players,
            input_size,
            check_distance,
            running: false,
            current_input: GameInput::new(NULL_FRAME, None, input_size),
            saved_frames: CircularBuffer::new(crate::MAX_PREDICTION_FRAMES as usize),
            sync_layer: SyncLayer::new(num_players, input_size),
        }
    }
}

impl GGEZSession for SyncTestSession {
    /// Must be called for each player in the session (e.g. in a 3 player session, must be called 3 times). Returns a playerhandle to identify the player in future method calls.
    fn add_player(&mut self, player: &Player) -> Result<PlayerHandle, GGEZError> {
        if player.player_handle > self.num_players as PlayerHandle {
            return Err(GGEZError::InvalidRequest);
        }
        Ok(player.player_handle)
    }

    /// After you are done defining and adding all players, you should start the session. In a sync test, starting the session saves the initial game state and sets running to true.
    /// If the session is already running, return an error.
    fn start_session(&mut self) -> Result<(), GGEZError> {
        match self.running {
            true => return Err(GGEZError::InvalidRequest),
            false => self.running = true,
        }
        self.current_frame = 0;
        Ok(())
    }

    /// Used to notify GGEZ of inputs that should be transmitted to remote players. add_local_input must be called once every frame for all players of type [PlayerType::Local].
    /// In the sync test, we don't send anything, we simply save the latest input.
    fn add_local_input(
        &mut self,
        player_handle: PlayerHandle,
        input: &[u8],
    ) -> Result<(), GGEZError> {
        // player handle is invalid
        if player_handle > self.num_players as PlayerHandle {
            return Err(GGEZError::InvalidPlayerHandle);
        }
        // session has not been started
        if !self.running {
            return Err(GGEZError::NotSynchronized);
        }
        // copy the local input bits into the current input
        self.current_input.copy_input(input);
        // update the current input to the right frame
        self.current_input.frame = self.current_frame;

        // send the input into the sync layer
        self.sync_layer
            .add_local_input(player_handle, &self.current_input)?;
        Ok(())
    }

    /// In a sync test, this will advance the state by a single frame and afterwards rollback "check_distance" amount of frames,
    /// resimulate and compare checksums with the original states. if checksums don't match, this will return [GGEZError::SyncTestFailed].
    fn advance_frame(&mut self, interface: &mut impl GGEZInterface) -> Result<(), GGEZError> {
        // save the current frame in the syncronization layer
        self.sync_layer
            .save_current_state(interface.save_game_state());

        // save a copy info in our separate queue so we have something to compare to later
        match self.sync_layer.get_last_saved_state() {
            Some(fi) => self.saved_frames.push_back(FrameInfo {
                frame: self.current_frame,
                state: fi.clone(),
                input: self.current_input.clone(),
            }),
            None => {
                return Err(GGEZError::GeneralFailure(String::from(
                    "sync layer did not return a last saved state",
                )));
            }
        };

        // get the correct inputs for all players from the sync layer
        let sync_inputs = self.sync_layer.get_synchronized_inputs();
        assert_eq!(sync_inputs[0].frame, self.sync_layer.get_current_frame());
        assert_eq!(sync_inputs[0].frame, self.current_frame);

        // advance the frame
        interface.advance_frame(sync_inputs, 0); 
        self.sync_layer.advance_frame();
        self.current_frame += 1;

        // current input has been used, so we can delete the input bits
        self.current_input.erase_bits();

        // manual simulated rollback section without using the sync_layer, but only if we have enough frames in the queue
        if self.saved_frames.len() > self.check_distance as usize {
            // load the frame that lies `check_distance` frames in the past
            let frame_to_load = self.current_frame - self.check_distance as i32;
            interface.load_game_state(self.sync_layer.load_frame(frame_to_load));

            // sanity check frame counts
            assert_eq!(self.sync_layer.get_current_frame(), frame_to_load);

            // resimulate the last frames
            for i in (0..self.check_distance).rev() {
                // let the sync layer save
                self.sync_layer.save_current_state(interface.save_game_state());

                // get the correct old frame info
                let pos_in_queue = self.saved_frames.len() - 1 - i as usize;
                let old_frame_info =
                    self.saved_frames
                        .get(pos_in_queue)
                        .ok_or(GGEZError::GeneralFailure(String::from(
                            "sync test could not load frame info from own queue",
                        )))?;
                // the frame we loaded should be from the correct frame
                assert_eq!(
                    old_frame_info.frame,
                    frame_to_load + (self.check_distance - 1 - i) as i32
                );

                // the current state should have the correct frame
                assert_eq!(self.sync_layer.get_current_frame(), old_frame_info.frame);

                // compare the checksums
                let last_saved_state = self.sync_layer.get_last_saved_state().unwrap();
                if let (Some(cs1), Some(cs2)) = (last_saved_state.checksum, old_frame_info.state.checksum)
                {
                    if cs1 != cs2 {
                        return Err(GGEZError::SyncTestFailed);
                    }
                }

                // advance the frame
                let sync_inputs = self.sync_layer.get_synchronized_inputs();
                self.sync_layer.advance_frame();                
                interface.advance_frame(sync_inputs, 0); 
            }
            // we should have arrived back at the current frame
            let gs_compare = interface.save_game_state();
            assert_eq!(gs_compare.frame, self.current_frame);
            assert_eq!(self.sync_layer.get_current_frame(), self.current_frame);

            // since this is a sync test, we "cheat" by setting the last confirmed state to the (current state - check_distance), so the sync layer wont complain about missing
            // inputs from other players
            self.sync_layer
                .set_last_confirmed_frame(self.current_frame - self.check_distance as i32);
        }

        // after all of this, the sync layer and our own frame_counting should match
        assert_eq!(self.sync_layer.get_current_frame(), self.current_frame);
        Ok(())
    }

    /// Nothing happens here in [SyncTestSession]. There are no packets to be received or sent and no rollbacks can occur other than the manually induced ones.
    fn idle(&self, _interface: &mut impl GGEZInterface) -> Result<(), GGEZError> {
        Ok(())
    }

    /// Sets the input delay for a given player to a given number.
    fn set_frame_delay(
        &mut self,
        frame_delay: u32,
        player_handle: PlayerHandle,
    ) -> Result<(), GGEZError> {
        if self.running {
            return Err(GGEZError::InvalidRequest);
        }
        self.sync_layer.set_frame_delay(player_handle, frame_delay);
        Ok(())
    }

    /// Not supported in [SyncTestSession].
    fn disconnect_player(&mut self, _player_handle: PlayerHandle) -> Result<(), GGEZError> {
        Err(GGEZError::Unsupported)
    }

    /// Not supported in [SyncTestSession].
    fn get_network_stats(&self, _player_handle: PlayerHandle) -> Result<NetworkStats, GGEZError> {
        Err(GGEZError::Unsupported)
    }

    /// Not supported in [SyncTestSession].
    fn set_disconnect_timeout(&self, _timeout: u32) -> Result<(), GGEZError> {
        Err(GGEZError::Unsupported)
    }

    /// Not supported in [SyncTestSession].
    fn set_disconnect_notify_delay(&self, _notify_delay: u32) -> Result<(), GGEZError> {
        Err(GGEZError::Unsupported)
    }
}

// #########
// # TESTS #
// #########

#[cfg(test)]
mod sync_test_session_tests {
    use crate::player::{Player, PlayerType};
    use crate::{GGEZError, GGEZSession};
    use bincode;

    #[test]
    fn test_add_player() {
        let mut sess = crate::start_synctest_session(1, 2, std::mem::size_of::<u32>());

        // add players correctly
        let dummy_player_0 = Player::new(PlayerType::Local, 0);
        let dummy_player_1 = Player::new(PlayerType::Local, 1);

        match sess.add_player(&dummy_player_0) {
            Ok(handle) => assert_eq!(handle, 0),
            Err(_) => assert!(false),
        }

        match sess.add_player(&dummy_player_1) {
            Ok(handle) => assert_eq!(handle, 1),
            Err(_) => assert!(false),
        }
    }

    #[test]
    fn test_add_player_invalid_handle() {
        let mut sess = crate::start_synctest_session(1, 2, std::mem::size_of::<u32>());

        // add a player incorrectly
        let incorrect_player = Player::new(PlayerType::Local, 3);

        match sess.add_player(&incorrect_player) {
            Err(GGEZError::InvalidRequest) => (),
            _ => assert!(false),
        }
    }

    #[test]
    fn test_add_local_input_not_running() {
        let mut sess = crate::start_synctest_session(1, 2, std::mem::size_of::<u32>());

        // add 0 input for player 0
        let fake_inputs: u32 = 0;
        let serialized_inputs = bincode::serialize(&fake_inputs).unwrap();

        match sess.add_local_input(0, &serialized_inputs) {
            Err(GGEZError::NotSynchronized) => (),
            _ => assert!(false),
        }
    }

    #[test]
    fn test_add_local_input_invalid_handle() {
        let mut sess = crate::start_synctest_session(1, 2, std::mem::size_of::<u32>());
        sess.start_session().unwrap();

        // add 0 input for player 3
        let fake_inputs: u32 = 0;
        let serialized_inputs = bincode::serialize(&fake_inputs).unwrap();

        match sess.add_local_input(3, &serialized_inputs) {
            Err(GGEZError::InvalidPlayerHandle) => (),
            _ => assert!(false),
        }
    }

    #[test]
    fn test_add_local_input() {
        let num_players: u32 = 2;
        let mut sess = crate::start_synctest_session(1, num_players, std::mem::size_of::<u32>());
        sess.start_session().unwrap();

        // add 0 input for player 0
        let fake_inputs: u32 = 0;
        let serialized_inputs = bincode::serialize(&fake_inputs).unwrap();

        match sess.add_local_input(0, &serialized_inputs) {
            Ok(()) => {
                for i in 0..sess.current_input.bits.len() {
                    assert_eq!(sess.current_input.bits[i], 0);
                }
            }
            Err(_e) => {
                assert!(false);
            }
        }

        // add 1 << 4 input for player 1, now the 5th byte should be 1 << 4
        let fake_inputs: u32 = 1 << 4;
        let serialized_inputs = bincode::serialize(&fake_inputs).unwrap();
        match sess.add_local_input(1, &serialized_inputs) {
            Ok(()) => {
                for i in 0..sess.current_input.bits.len() {
                    if i == 0 {
                        assert_eq!(sess.current_input.bits[i], 16);
                    } else {
                        assert_eq!(sess.current_input.bits[i], 0);
                    }
                }
            }
            _ => assert!(false),
        }
    }
}
