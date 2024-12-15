#[derive(Clone, Copy, Debug)]
pub enum Action {
    Play(u8),
    Pause(u8),
    Stop,
}
