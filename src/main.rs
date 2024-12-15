#![feature(new_uninit)]
#![feature(arbitrary_self_types)]
#![feature(let_chains)]

mod action;
mod output;
mod play_song;
mod read_cd;
mod state;

use std::{
    os::unix::prelude::AsRawFd,
    sync::{Arc, Mutex},
    thread,
};

use std::convert::TryInto;

use calloop::{generic::Generic, Interest, PostAction};
use color_eyre::{eyre::Context, Result};
use log::{info, warn};
use play_song::Player;
use read_cd::Reader;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_output, delegate_registry, delegate_seat,
    delegate_shm, delegate_xdg_shell, delegate_xdg_window,
    event_loop::WaylandSource,
    output::{OutputHandler, OutputState},
    reexports::client::{
        protocol::{wl_keyboard, wl_output, wl_seat, wl_shm, wl_surface},
        Connection, QueueHandle,
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Modifiers},
        Capability, SeatHandler, SeatState,
    },
    shell::xdg::{
        window::{Window, WindowConfigure, WindowHandler, XdgWindowState},
        XdgShellHandler, XdgShellState,
    },
    shm::{
        slot::{Buffer, SlotPool},
        ShmHandler, ShmState,
    },
};
use state::Request;
use udev::MonitorBuilder;
use zbus::dbus_interface;

use crate::{action::Action, state::PlayerState};

struct MprisInterface;

#[dbus_interface(name = "org.mpris.MediaPlayer2")]
impl MprisInterface {}

struct MprisPlayerInterface {
    player_state: Arc<Mutex<PlayerState>>,
}

#[dbus_interface(name = "org.mpris.MediaPlayer2.Player")]
impl MprisPlayerInterface {
    async fn next(&self) {
        self.player_state
            .lock()
            .unwrap()
            .handle_request(Request::NextTrack);
    }

    async fn play_pause(&self) {
        println!("HERE");
        self.player_state
            .lock()
            .unwrap()
            .handle_request(Request::TogglePlay);
    }
}

fn main() -> Result<()> {
    env_logger::init();

    let mut socket = MonitorBuilder::new()
        .context("monitor build failed")?
        .match_subsystem("block")
        .context("subsystem filter failed")?
        // .match_tag("ID_CDROM_CD_R=1")
        // .context("tag filter failed")?
        .listen()
        .context("udev monitor listen failed")?;

    let (tx, rx) = flume::bounded(2);

    let state = Arc::new(Mutex::new(PlayerState::new(tx, rx)));

    let mpris_player = MprisPlayerInterface {
        player_state: state.clone(),
    };
    let dbus = zbus::blocking::ConnectionBuilder::session()?
        .serve_at("/org/mpris/MediaPlayer2", MprisInterface)?
        .serve_at("/org/mpris/MediaPlayer2/Player", mpris_player)?
        .build()?;
    dbus.request_name("org.mpris.MediaPlayer2.raspicdplayer")?;

    std::fs::create_dir_all("/tmp/raspi-cd-player").unwrap();

    let spawn_player = |state| {
        thread::spawn(|| {
            let rtry = || -> Result<()> {
                let mut player = Player::new(state)?;
                player.handle()?;
                Ok(())
            };
            if let Err(err) = rtry() {
                warn!("{err}");
            }
        })
    };

    let spawn_reader = |state| {
        thread::spawn(|| {
            let rtry = || -> Result<()> {
                let mut reader = Reader::new(state)?;
                reader.handle()?;
                Ok(())
            };
            if let Err(err) = rtry() {
                warn!("{err}");
            }
        })
    };

    let (mut reader_thread, mut player_thread) = if Reader::get_drive().is_some() {
        (
            Some(spawn_player(state.clone())),
            Some(spawn_reader(state.clone())),
        )
    } else {
        state.lock().unwrap().change_action(Action::Stop);
        (None, None)
    };

    let conn = Connection::connect_to_env().unwrap();

    let event_queue = conn.new_event_queue();
    let qh = event_queue.handle();

    let mut event_loop = calloop::EventLoop::<SimpleWindow>::try_new()?;

    WaylandSource::new(event_queue)?
        .insert(event_loop.handle())
        .unwrap();

    event_loop
        .handle()
        .insert_source(
            Generic::new(socket.as_raw_fd(), Interest::READ, calloop::Mode::Edge),
            |_, _, _| Ok(PostAction::Continue),
        )
        .unwrap();

    let mut simple_window = SimpleWindow {
        registry_state: RegistryState::new(&conn, &qh),
        seat_state: SeatState::new(),
        output_state: OutputState::new(),
        compositor_state: CompositorState::new(),
        shm_state: ShmState::new(),
        xdg_shell_state: XdgShellState::new(),
        xdg_window_state: XdgWindowState::new(),

        exit: false,
        first_configure: true,
        pool: None,
        width: 256,
        height: 256,
        buffer: None,
        window: None,
        keyboard: None,

        player_state: state.clone(),
    };

    while !simple_window.registry_state.ready() {
        event_loop.dispatch(None, &mut simple_window).unwrap();
    }

    let pool = SlotPool::new(
        simple_window.width as usize * simple_window.height as usize * 4,
        &simple_window.shm_state,
    )
    .expect("Failed to create pool");
    simple_window.pool = Some(pool);

    let surface = simple_window.compositor_state.create_surface(&qh).unwrap();

    let window = Window::builder()
        .title("raspi-cd-player")
        // GitHub does not let projects use the `org.github` domain but the `io.github` domain is fine.
        .min_size((256, 256))
        .map(
            &qh,
            &simple_window.xdg_shell_state,
            &mut simple_window.xdg_window_state,
            surface,
        )
        .expect("window creation");

    simple_window.window = Some(window);

    // We don't draw immediately, the configure will notify us when to first draw.

    loop {
        event_loop.dispatch(None, &mut simple_window).unwrap();

        if let Some(udev_event) = socket.next() && udev_event.devnode().unwrap().ends_with("sr0") {
            while socket.next().is_some() {}
            if Reader::get_drive().is_some() {
                state.lock().unwrap().change_action(Action::Stop);
                if let Some(thread) = reader_thread {
                    thread.join();
                }
                if let Some(thread) = player_thread {
                    thread.join();
                }

                state.lock().unwrap().change_action(Action::Play(1));
                player_thread = Some(spawn_player(state.clone()));
                reader_thread = Some(spawn_reader(state.clone()));
            } else {
                // The cd has been removed
                state.lock().unwrap().change_action(Action::Stop);
                if let Some(thread) = reader_thread {
                    thread.join();
                }
                if let Some(thread) = player_thread {
                    thread.join();
                }
                reader_thread = None;
                player_thread= None;
            }
        }

        if simple_window.exit {
            info!("exiting");
            state.lock().unwrap().change_action(Action::Stop);
            if let Some(thread) = reader_thread {
                thread.join();
            }
            if let Some(thread) = player_thread {
                thread.join();
            }
            break;
        }
    }

    Ok(())
}

struct SimpleWindow {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    compositor_state: CompositorState,
    shm_state: ShmState,
    xdg_shell_state: XdgShellState,
    xdg_window_state: XdgWindowState,

    exit: bool,
    first_configure: bool,
    pool: Option<SlotPool>,
    width: u32,
    height: u32,
    buffer: Option<Buffer>,
    window: Option<Window>,
    keyboard: Option<wl_keyboard::WlKeyboard>,

    player_state: Arc<Mutex<PlayerState>>,
}

impl CompositorHandler for SimpleWindow {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
        // Not needed for this example.
    }

    fn frame(
        &mut self,
        conn: &Connection,
        qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        self.draw(conn, qh);
    }
}

impl OutputHandler for SimpleWindow {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl XdgShellHandler for SimpleWindow {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }
}

impl WindowHandler for SimpleWindow {
    fn xdg_window_state(&mut self) -> &mut XdgWindowState {
        &mut self.xdg_window_state
    }

    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &Window) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        conn: &Connection,
        qh: &QueueHandle<Self>,
        _window: &Window,
        configure: WindowConfigure,
        _serial: u32,
    ) {
        match configure.new_size {
            Some(size) => {
                self.width = size.0;
                self.height = size.1;
                self.buffer = None;
            }
            None => {
                self.width = 256;
                self.height = 256;
                self.buffer = None;
            }
        }

        // Initiate the first draw.
        if self.first_configure {
            self.first_configure = false;
            self.draw(conn, qh);
        }
    }
}

impl SeatHandler for SimpleWindow {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            let keyboard = self
                .seat_state
                .get_keyboard(qh, &seat, None)
                .expect("Failed to create keyboard");
            self.keyboard = Some(keyboard);
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_some() {
            self.keyboard.take().unwrap().release();
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for SimpleWindow {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _keysyms: &[u32],
    ) {
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _: u32,
    ) {
    }

    fn press_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        // println!("{:?}", event);
        let req = match event {
            KeyEvent {
                time: _,
                raw_code: _,
                keysym: _,
                utf8,
            } => match utf8 {
                Some(key) => match key.as_str() {
                    " " => Request::TogglePlay,
                    "<" => Request::PreviousTrack,
                    ">" => Request::NextTrack,
                    "q" => Request::Quit,
                    &_ => Request::None,
                },
                None => Request::None,
            },
        };

        if matches!(req, Request::Quit) {
            self.exit = true;
        }
        self.player_state.lock().unwrap().handle_request(req);
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _serial: u32,
        modifiers: Modifiers,
    ) {
    }
}

impl ShmHandler for SimpleWindow {
    fn shm_state(&mut self) -> &mut ShmState {
        &mut self.shm_state
    }
}

impl SimpleWindow {
    pub fn draw(&mut self, _conn: &Connection, qh: &QueueHandle<Self>) {
        if let Some(window) = self.window.as_ref() {
            let width = self.width;
            let height = self.height;
            let stride = self.width as i32 * 4;
            let pool = self.pool.as_mut().unwrap();

            if self.buffer.is_none() {
                self.buffer = Some(
                    pool.create_buffer(
                        width as i32,
                        height as i32,
                        stride,
                        wl_shm::Format::Argb8888,
                    )
                    .expect("create buffer")
                    .0,
                );
                let buffer = self.buffer.as_ref().unwrap();

                let canvas = pool.canvas(buffer).unwrap();

                canvas
                    .chunks_exact_mut(4)
                    .enumerate()
                    .for_each(|(_, chunk)| {
                        let a = 0xFF;
                        let r = 0;
                        let g = 0;
                        let b = 0;
                        let color: i32 = (a << 24) + (r << 16) + (g << 8) + b;

                        let array: &mut [u8; 4] = chunk.try_into().unwrap();
                        *array = color.to_le_bytes();
                    });

                // Damage the entire window
                window
                    .wl_surface()
                    .damage_buffer(0, 0, self.width as i32, self.height as i32);

                // Request our next frame
                window.wl_surface().frame(qh, window.wl_surface().clone());

                // Attach and commit to present.
                buffer
                    .attach_to(window.wl_surface())
                    .expect("buffer attach");
                window.wl_surface().commit();
            }
        }
    }
}

delegate_compositor!(SimpleWindow);
delegate_output!(SimpleWindow);
delegate_shm!(SimpleWindow);

delegate_seat!(SimpleWindow);
delegate_keyboard!(SimpleWindow);

delegate_xdg_shell!(SimpleWindow);
delegate_xdg_window!(SimpleWindow);

delegate_registry!(SimpleWindow);

impl ProvidesRegistryState for SimpleWindow {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![
        CompositorState,
        OutputState,
        ShmState,
        SeatState,
        XdgShellState,
        XdgWindowState,
    ];
}
