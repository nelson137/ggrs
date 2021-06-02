use crate::error::GGRSError;
use crate::frame_info::GameInput;
use crate::frame_info::{GameState, BLANK_STATE};
use crate::input_queue::InputQueue;
use crate::{FrameNumber, PlayerHandle, MAX_INPUT_DELAY, MAX_PREDICTION_FRAMES, NULL_FRAME};
#[derive(Debug, Clone)]
pub(crate) struct SavedStates<T> {
    pub states: [T; MAX_PREDICTION_FRAMES as usize],
    pub head: usize,
}

impl<T> SavedStates<T> {
    pub(crate) fn save_state(&mut self, state_to_save: T) {
        self.head = (self.head + 1) % self.states.len();
        self.states[self.head] = state_to_save;
    }

    pub(crate) const fn state_at_head(&self) -> &T {
        &self.states[self.head]
    }

    pub(crate) fn state_in_past(&self, frames_in_past: usize) -> &T {
        let pos =
            (self.head as i64 - frames_in_past as i64).rem_euclid(MAX_PREDICTION_FRAMES as i64);
        assert!(pos >= 0);
        &self.states[pos as usize]
    }
}

#[derive(Debug)]
pub(crate) struct SyncLayer {
    num_players: u32,
    input_size: usize,
    saved_states: SavedStates<GameState>,
    rolling_back: bool,
    last_confirmed_frame: FrameNumber,
    current_frame: FrameNumber,
    input_queues: Vec<InputQueue>,
}

impl SyncLayer {
    /// Creates a new `SyncLayer` instance with given values.
    pub(crate) fn new(num_players: u32, input_size: usize) -> Self {
        // initialize input_queues
        let mut input_queues = Vec::new();
        for i in 0..num_players {
            input_queues.push(InputQueue::new(i as PlayerHandle, input_size));
        }
        Self {
            num_players,
            input_size,
            rolling_back: false,
            last_confirmed_frame: -1,
            current_frame: 0,
            saved_states: SavedStates {
                head: 0,
                states: [BLANK_STATE; MAX_PREDICTION_FRAMES as usize],
            },
            input_queues,
        }
    }

    pub(crate) const fn current_frame(&self) -> FrameNumber {
        self.current_frame
    }

    pub(crate) fn advance_frame(&mut self) {
        self.current_frame += 1;
    }

    pub(crate) fn save_current_state(&mut self, state_to_save: GameState) {
        assert!(state_to_save.frame != NULL_FRAME);
        self.saved_states.save_state(state_to_save)
    }

    pub(crate) const fn last_saved_state(&self) -> Option<&GameState> {
        match self.saved_states.state_at_head().frame {
            NULL_FRAME => None,
            _ => Some(self.saved_states.state_at_head()),
        }
    }

    pub(crate) fn set_frame_delay(&mut self, player_handle: PlayerHandle, delay: u32) {
        assert!(player_handle < self.num_players as PlayerHandle);
        assert!(delay <= MAX_INPUT_DELAY);

        self.input_queues[player_handle as usize].set_frame_delay(delay);
    }

    pub(crate) fn reset_prediction(&mut self, frame: FrameNumber) {
        for i in 0..self.num_players {
            self.input_queues[i as usize].reset_prediction(frame);
        }
    }

    /// Loads the gamestate indicated by `frame_to_load`. After execution, `self.saved_states.head` is set one position after the loaded state.
    pub(crate) fn load_frame(&mut self, frame_to_load: FrameNumber) -> &GameState {
        // The state should not be the current state or the state should not be in the future or too far away in the past
        assert!(
            frame_to_load != NULL_FRAME
                && frame_to_load < self.current_frame
                && frame_to_load >= self.current_frame - MAX_PREDICTION_FRAMES as i32
        );

        self.saved_states.head = self.find_saved_frame_index(frame_to_load);
        let state_to_load = &self.saved_states.states[self.saved_states.head];
        assert_eq!(state_to_load.frame, frame_to_load);

        // Reset framecount and the head of the state ring-buffer to point in
        // advance of the current frame (as if we had just finished executing it).
        self.saved_states.head = (self.saved_states.head + 1) % MAX_PREDICTION_FRAMES as usize;
        self.current_frame = frame_to_load;

        state_to_load
    }

    /// Adds local input to the corresponding input queue. Checks if the prediction threshold has been reached. Returns the frame number where the input is actually added to.
    /// This number will only be different if the input delay was set to a number higher than 0.
    pub(crate) fn add_local_input(
        &mut self,
        player_handle: PlayerHandle,
        input: GameInput,
    ) -> Result<FrameNumber, GGRSError> {
        let frames_behind = self.current_frame - self.last_confirmed_frame;
        if frames_behind > MAX_PREDICTION_FRAMES as i32 {
            return Err(GGRSError::PredictionThreshold);
        }

        // The input provided should match the current frame
        assert_eq!(input.frame, self.current_frame);
        Ok(self.input_queues[player_handle].add_input(input))
    }

    /// Adds remote input to the correspoinding input queue.
    /// Unlike `add_local_input`, this will not check for correct conditions, as remote inputs have already been checked on another device.
    pub(crate) fn add_remote_input(&mut self, player_handle: PlayerHandle, input: GameInput) {
        self.input_queues[player_handle].add_input(input);
    }

    /// Returns inputs for all players for the current frame of the sync layer. If there are none for a specific player, return predictions.
    pub(crate) fn synchronized_inputs(&mut self) -> Vec<GameInput> {
        let mut inputs = Vec::new();
        for i in 0..self.num_players {
            inputs.push(self.input_queues[i as usize].input(self.current_frame));
        }
        inputs
    }

    /// Returns confirmed inputs for all players for the current frame of the sync layer.
    pub(crate) fn confirmed_inputs(&mut self) -> Vec<GameInput> {
        let mut inputs = Vec::new();
        for i in 0..self.num_players {
            inputs.push(self.input_queues[i as usize].confirmed_input(self.current_frame as u32));
        }
        inputs
    }

    /// Sets the last confirmed frame to a given frame. By raising the last confirmed frame, we can discard all previous frames, as they are no longer necessary.
    pub(crate) fn set_last_confirmed_frame(&mut self, frame: FrameNumber) {
        self.last_confirmed_frame = frame;
        if self.last_confirmed_frame > 0 {
            for i in 0..self.num_players {
                self.input_queues[i as usize].discard_confirmed_frames(frame - 1);
            }
        }
    }

    /// Searches the saved states and returns the index of the state that matches the given frame number.
    fn find_saved_frame_index(&self, frame: FrameNumber) -> usize {
        for i in 0..MAX_PREDICTION_FRAMES as usize {
            if self.saved_states.states[i].frame == frame {
                return i;
            }
        }
        panic!("SyncLayer::find_saved_frame_index(): requested state could not be found");
    }
}

// #########
// # TESTS #
// #########

#[cfg(test)]
mod sync_layer_tests {

    use super::*;

    #[test]
    #[should_panic]
    fn test_reach_prediction_threshold() {
        let mut sync_layer = SyncLayer::new(2, std::mem::size_of::<u32>());
        for i in 0..20 {
            let serialized_input = bincode::serialize(&i).unwrap();
            let mut game_input = GameInput::new(i, None, std::mem::size_of::<u32>());
            game_input.copy_input(&serialized_input);
            sync_layer.add_local_input(0, game_input).unwrap(); // should crash at frame 7
        }
    }

    #[test]
    fn test_different_delays() {
        let mut sync_layer = SyncLayer::new(2, std::mem::size_of::<u32>());
        let p1_delay = 2;
        let p2_delay = 0;
        sync_layer.set_frame_delay(0, p1_delay);
        sync_layer.set_frame_delay(1, p2_delay);

        for i in 0..20 {
            let serialized_input = bincode::serialize(&i).unwrap();
            let mut game_input = GameInput::new(i, None, std::mem::size_of::<u32>());
            game_input.copy_input(&serialized_input);
            // adding input as remote to avoid prediction threshold detection
            sync_layer.add_remote_input(0, game_input);
            sync_layer.add_remote_input(1, game_input);

            if i >= 3 {
                let sync_inputs = sync_layer.synchronized_inputs();
                let player0_inputs: u32 = bincode::deserialize(&sync_inputs[0].bits).unwrap();
                let player1_inputs: u32 = bincode::deserialize(&sync_inputs[1].bits).unwrap();
                assert_eq!(player0_inputs, i as u32 - p1_delay);
                assert_eq!(player1_inputs, i as u32 - p2_delay);
            }

            sync_layer.advance_frame();
        }
    }
}