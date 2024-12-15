use std::sync::{Arc, Mutex};

use flume::{Receiver, Sender};
use std::sync::{MutexGuard, RwLock};

use crate::action::Action;

pub enum Request {
    TogglePlay,
    NextTrack,
    PreviousTrack,
    SeekForward,
    SeekBackward,
    None,
    Quit,
}

pub struct PlayerState {
    pub action: Action,
    pub state_changed: Arc<RwLock<bool>>,
    pub total_tracks: u8,
    changed: Sender<()>,
    wait_change: Receiver<()>,
}

unsafe impl Sync for PlayerState {}

impl PlayerState {
    pub fn new(tx: Sender<()>, rx: Receiver<()>) -> Self {
        Self {
            action: Action::Play(1),
            state_changed: Arc::new(RwLock::new(false)),
            changed: tx,
            wait_change: rx,
            total_tracks: 0,
        }
    }
    pub fn wait_for_change(self: MutexGuard<Self>) {
        let wait_change = self.wait_change.clone();
        drop(self);
        wait_change.recv().unwrap();
    }

    pub fn change_action(mut self: MutexGuard<Self>, action: Action) {
        self.action = action;
        *self.state_changed.write().unwrap() = true;
        let _ = self.changed.try_send(());
        let _ = self.changed.try_send(());
    }

    pub fn next_track(self: MutexGuard<Self>) {
        match self.action {
            Action::Play(track) | Action::Pause(track) => {
                let next_track = track + 1;
                let action = if next_track < self.total_tracks.into() {
                    Action::Play(next_track)
                } else {
                    // The CD has finished
                    Action::Stop
                };
                self.change_action(action);
            }
            Action::Stop => {}
        }
    }

    pub fn prev_track(self: MutexGuard<Self>) {
        match self.action {
            Action::Play(track) | Action::Pause(track) => {
                let prev_track = track - 1;
                let track_to_play = if prev_track >= 1 { prev_track } else { track };
                self.change_action(Action::Play(track_to_play));
            }
            Action::Stop => {}
        }
    }

    pub fn handle_request(self: MutexGuard<Self>, req: Request) {
        match req {
            Request::TogglePlay => match self.action {
                Action::Play(track) => {
                    self.change_action(Action::Pause(track));
                }
                Action::Pause(track) => {
                    self.change_action(Action::Play(track));
                }
                Action::Stop => todo!(),
            },
            Request::NextTrack => {
                self.next_track();
            }
            Request::PreviousTrack => {
                self.prev_track();
            }
            Request::SeekForward => todo!(),
            Request::SeekBackward => todo!(),
            Request::None => {}
            Request::Quit => {}
        }
    }
}
