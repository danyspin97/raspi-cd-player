use std::{
    fs::File,
    path::PathBuf,
    sync::{Arc, Mutex},
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
    state::PlayerState,
};

pub struct Player {
    format: WavReader,
    decoder: Box<dyn Decoder>,
    audio_output: Box<dyn AudioOutput>,
    state: Arc<Mutex<PlayerState>>,
    file: File,
}

impl Player {
    pub fn new(state: Arc<Mutex<PlayerState>>) -> Result<Self> {
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

        let (file, format) = Self::get_reader(1);

        // song_is_ready.recv().unwrap();
        Ok(Self {
            format,
            decoder,
            audio_output,
            state,
            file,
        })
    }

    pub fn handle(&mut self) -> Result<()> {
        loop {
            let action = {
                let lock = self.state.lock().unwrap();
                lock.action.clone()
            };

            match action {
                Action::Play(track) => {
                    (self.file, self.format) = Self::get_reader(track.into());
                    // The song finished playing by itself
                    if self.play() {
                        let state = self.state.lock().unwrap();
                        state.next_track();
                    }
                }
                Action::Pause(_) => {
                    self.state.lock().unwrap().wait_for_change();
                }
                Action::Stop => break,
            }
        }

        Ok(())
    }

    pub fn play(&mut self) -> bool {
        // Wait until there is enough data to read
        while self.file.metadata().unwrap().len() < 1152 * 2 {
            std::thread::sleep(Duration::from_millis(5));
        }
        let state_changed = self.state.lock().unwrap().state_changed.clone();
        let song_finished = loop {
            if *state_changed.read().unwrap() {
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

    fn get_reader(id: usize) -> (File, WavReader) {
        let filename = PathBuf::from(format!("/tmp/raspi-cd-player/track{id}"));
        // wait for the file to be created
        while !filename.exists() {
            std::thread::sleep(Duration::from_millis(20));
        }
        let file = File::open(filename).unwrap();
        let source = Box::new(file.try_clone().unwrap());
        let mss = MediaSourceStream::new(source, Default::default());
        let format_opts = FormatOptions::default();
        (file, WavReader::try_new(mss, &format_opts).unwrap())
    }
}
