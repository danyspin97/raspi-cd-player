use std::sync::{atomic::AtomicBool, Arc, Barrier};

use flume::{Receiver, Sender};

use crate::action::Action;

pub struct State {
    pub action: Action,
    pub state_changed: Arc<AtomicBool>,
    pub track_to_play: usize,
    pub total_tracks: u8,
    pub action_read: Barrier,
    pub changed: Sender<()>,
    pub wait_change: Receiver<()>,
}

unsafe impl Sync for State {}
