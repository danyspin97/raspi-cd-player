#![feature(new_uninit)]
#![feature(read_buf)]

use std::{
    cell::RefCell,
    sync::{atomic::AtomicBool, Arc, Barrier, Mutex},
    thread,
};
mod action;
mod output;
mod play_song;
mod read_cd;
mod state;

use action::Action;
use color_eyre::{eyre::Context, Result};
use play_song::Player;
use read_cd::Reader;
use udev::MonitorBuilder;

use state::State;

fn main() -> Result<()> {
    let socket = MonitorBuilder::new()
        .context("monitor build failed")?
        .match_subsystem_devtype("block", "disk")
        .context("subsystem filter failed")?
        .match_tag("ID_CDROM_CD_R=1")
        .context("tag filter failed")?
        .listen()
        .context("udev monitor listen failed")?;

    let (tx, rx) = flume::bounded(2);

    let state = Arc::new(Mutex::new(State {
        action: Action::Play,
        state_changed: Arc::new(AtomicBool::new(false)),
        track_to_play: 2,
        action_read: Barrier::new(3),
        changed: tx,
        wait_change: rx,
        total_tracks: 0,
    }));

    std::fs::create_dir_all("/tmp/raspi-cd-player").unwrap();

    let state_clone = state.clone();
    let handle_read = thread::spawn(|| {
        let mut reader = Reader::new(state_clone).unwrap();
        reader.handle().unwrap();
    });

    let handle_play = thread::spawn(|| {
        let mut player = Player::new(state).unwrap();
        println!("player finished building");
        player.handle().unwrap();
    });

    handle_read.join().unwrap();
    handle_play.join().unwrap();

    Ok(())
}
