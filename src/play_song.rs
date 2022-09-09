use std::{
    fs::File,
    path::PathBuf,
    sync::{atomic::Ordering::Relaxed, Arc, Mutex},
    time::Duration,
};

use color_eyre::Result;
use symphonia::core::{
    audio::{Channels, SignalSpec},
    codecs::{CodecParameters, Decoder, DecoderOptions, CODEC_TYPE_PCM_S16LE},
    formats::{FormatOptions, FormatReader},
    io::MediaSourceStream,
};
use symphonia_format_wav::WavReader;

use crate::{
    action::Action,
    output::{self, AudioOutput},
    state::State,
};

pub struct Player {
    format: WavReader,
    decoder: Box<dyn Decoder>,
    audio_output: Box<dyn AudioOutput>,
    state: Arc<Mutex<State>>,
}

impl Player {
    pub fn new(state: Arc<Mutex<State>>) -> Result<Self> {
        let mut codec_params = CodecParameters::new();
        codec_params
            .for_codec(CODEC_TYPE_PCM_S16LE)
            .with_sample_rate(44100)
            .with_time_base(symphonia::core::units::TimeBase {
                numer: 1,
                denom: 44100,
            })
            .with_bits_per_sample(16)
            .with_channels(Channels::FRONT_LEFT | Channels::FRONT_RIGHT)
            .with_max_frames_per_packet(1152);
        let decode_opts = DecoderOptions::default();
        let decoder = symphonia::default::get_codecs().make(&codec_params, &decode_opts)?;

        // This is a description of the audio buffer's sample format
        // and sample rate.
        let spec = SignalSpec::new(44100, Channels::FRONT_LEFT | Channels::FRONT_RIGHT);

        // Try to open the audio output.
        let audio_output = output::try_open(spec, 1152).unwrap();

        let format = Self::get_reader(1);

        // song_is_ready.recv().unwrap();
        Ok(Self {
            format,
            decoder,
            audio_output,
            state,
        })
    }

    pub fn handle(&mut self) -> Result<()> {
        loop {
            println!("player: HERE1");
            let lock = self.state.lock().unwrap();
            let action = lock.action.clone();
            drop(lock);
            println!("player: HERE2");
            println!("player: HERE2.2");
            self.state.lock().unwrap().action_read.wait();
            println!("player: HERE3");
            match action {
                Action::Play => {
                    // The song finished playing by itself
                    if self.play() {
                        let mut state = self.state.lock().unwrap();
                        let previous = state.state_changed.swap(true, Relaxed);
                        // The state hasn't been changed between the previous two lines
                        if !previous {
                            if state.track_to_play + 1 < state.total_tracks.into() {
                                state.action = Action::Change;
                                self.format = Self::get_reader(state.track_to_play + 1);
                            } else {
                                // The CD has finished
                                state.action = Action::Stop;
                                // Wakeup the reader thread
                                state.changed.send(()).unwrap();
                                // Immediately acknowledge that we have read the new action
                                state.action_read.wait();
                                // And exit
                                break;
                            }
                        }
                    }
                }
                Action::Pause => todo!(),
                Action::Stop => break,
                Action::Change => {
                    self.format = Self::get_reader(self.state.lock().unwrap().track_to_play);
                    self.state.lock().unwrap().wait_change.recv().unwrap();
                }
            }
        }

        Ok(())
    }

    pub fn play(&mut self) -> bool {
        let state_changed = self.state.lock().unwrap().state_changed.clone();
        let song_finished = loop {
            if state_changed.load(Relaxed) {
                break false;
            }
            // Get the next packet from the format reader.
            let packet = match self.format.next_packet() {
                Ok(packet) => packet,
                Err(_err) => break true,
            };

            // Decode the packet into audio samples.
            let decoded = self.decoder.decode(&packet).unwrap();

            self.audio_output.write(decoded).unwrap()
        };

        // Flush the audio output to finish playing back any leftover samples.
        self.audio_output.flush();
        song_finished
    }

    fn get_reader(id: usize) -> WavReader {
        let filename = PathBuf::from(format!("/tmp/raspi-cd-player/track{id}"));
        // wait for the file to be created
        while !filename.exists() {
            std::thread::sleep(Duration::from_millis(20));
        }
        let file = File::open(filename).unwrap();
        // // Wait until there is enough data to read
        // while file.metadata().unwrap().len() < 1152 * 2 {
        //     std::thread::sleep(Duration::from_millis(20));
        // }
        let source = Box::new(file);
        let mss = MediaSourceStream::new(source, Default::default());
        let format_opts = FormatOptions::default();
        WavReader::try_new(mss, &format_opts).unwrap()
    }
}
