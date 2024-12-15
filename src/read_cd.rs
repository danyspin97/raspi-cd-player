use std::{
    fs::File,
    io::{BufWriter, Write},
    mem::MaybeUninit,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use color_eyre::{
    eyre::{bail, ContextCompat},
    Result,
};
use libcdio_sys::*;

use crate::{action::Action, state::PlayerState};

pub struct Song {
    filename: PathBuf,
    file: File,
    pub offset: i32,
    pub track_id: usize,
    pub start_lsn: i32,
    pub end_lsn: i32,
    pub ended: bool,
}

impl Song {
    pub fn new(track_id: usize, (start_lsn, end_lsn): (i32, i32)) -> Result<Self> {
        let filename = PathBuf::from(format!("/tmp/raspi-cd-player/track{}", track_id));
        // TODO: Use create_now once it stabilizes
        let file = File::create(&filename)?;
        let mut song = Self {
            filename,
            file,
            offset: 0,
            track_id,
            start_lsn,
            end_lsn,
            ended: false,
        };

        let bytes = CDIO_CD_FRAMESIZE_RAW * (end_lsn - start_lsn) as u32;
        song.write_wav_header(bytes)?;

        Ok(song)
    }

    pub fn read(&mut self, cdio: *mut _CdIo, state: Arc<Mutex<PlayerState>>) -> Result<()> {
        const SEC: u32 = 52;

        let mut curr = self.start_lsn + self.offset;
        let mut writer = BufWriter::new(&self.file);
        let state_changed = state.lock().unwrap().state_changed.clone();
        while curr < self.end_lsn && !*state_changed.read().unwrap() {
            let mut buf = [0; (CDIO_CD_FRAMESIZE_RAW * SEC) as usize];
            unsafe {
                if cdio_read_audio_sectors(
                    cdio,
                    buf.as_mut_ptr() as *mut std::ffi::c_void,
                    curr,
                    SEC,
                ) != driver_return_code_t_DRIVER_OP_SUCCESS
                {
                    bail!("error reading sector");
                }
            }
            curr += (self.end_lsn - curr) % SEC as i32 + 1;
            writer.write(&buf).unwrap();
        }

        if curr >= self.end_lsn {
            // The song has completely read
            self.ended = true;
        } else {
            // The reading has been interrupted
            self.offset = curr;
        }

        writer.flush().unwrap();

        Ok(())
    }

    fn write_wav_header(&mut self, bytes: u32) -> Result<()> {
        const BITDEPTH: u16 = 16;
        const SAMPLERATE: u32 = 44100;
        const CHANNELS: u16 = 2;
        const BLOCKALIGN: u16 = 4;
        const BYTERATE: u32 = SAMPLERATE * BITDEPTH as u32 / 8;
        const FORMAT: u16 = 1; // WAVE_FORMAT_PCM
        const CHUNKSIZE: u32 = 16;

        self.file.write_all("RIFF".as_bytes())?;
        // This is the file size
        // 44 is the header size
        self.file.write_all(&(bytes + 44 - 8).to_le_bytes())?;
        self.file.write_all("WAVE".as_bytes())?;

        //  Format
        self.file.write_all("fmt ".as_bytes())?;
        self.file.write_all(&CHUNKSIZE.to_le_bytes())?;
        self.file.write_all(&FORMAT.to_le_bytes())?;
        self.file.write_all(&CHANNELS.to_le_bytes())?;
        self.file.write_all(&SAMPLERATE.to_le_bytes())?;
        self.file.write_all(&BYTERATE.to_le_bytes())?;
        self.file.write_all(&BLOCKALIGN.to_le_bytes())?;
        self.file.write_all(&BITDEPTH.to_le_bytes())?;

        // Data
        self.file.write_all("data".as_bytes())?;
        self.file.write_all(&bytes.to_le_bytes())?;

        self.file.flush()?;

        Ok(())
    }
}

impl Drop for Song {
    fn drop(&mut self) {
        std::fs::remove_file(&self.filename).unwrap();
    }
}

pub struct Reader {
    cdio: *mut _CdIo,
    song_sectors: Vec<(i32, i32)>,
    tracks: u8,
    state: Arc<Mutex<PlayerState>>,
    songs: Vec<Song>,
}

impl Reader {
    pub fn new(state: Arc<Mutex<PlayerState>>) -> Result<Self> {
        let driver_id = Box::new(driver_id_t_DRIVER_LINUX);
        let drive = Reader::get_drive().context("Can't find a CD-ROM drive with a CD-DA in it")?;
        let cdio = unsafe { cdio_open(drive, *driver_id) };
        unsafe {
            cdio_set_speed(cdio, 1);
        }

        let first_track = unsafe { cdio_get_first_track_num(cdio) };
        let last_track = unsafe { cdio_get_last_track_num(cdio) };
        let tracks = unsafe { cdio_get_num_tracks(cdio) };

        if first_track == 0xFF || last_track == 0xFF {
            bail!("invalid CD");
        }
        let mut toc: Box<[MaybeUninit<msf_t>]> = Box::new_uninit_slice(0xAA + 1);

        for current_track in first_track..=last_track {
            if unsafe {
                cdio_get_track_msf(
                    cdio,
                    current_track,
                    toc.get_mut(current_track as usize).unwrap().as_mut_ptr(),
                )
            } == 0
            {
                bail!("error reading cd");
            }

            // if unsafe { cdio_get_track_format(cdio, current_track) }
            //     == track_format_t_TRACK_FORMAT_AUDIO
            // {
            //     if current_track != first_track {
            //         let s = unsafe {
            //             cdio_audio_get_msf_seconds(
            //                 toc.get_mut(current_track as usize).unwrap().as_mut_ptr(),
            //             ) - cdio_audio_get_msf_seconds(
            //                 toc.get_mut((current_track - 1) as usize)
            //                     .unwrap()
            //                     .as_mut_ptr(),
            //             )
            //         };
            //     }
            // }
        }

        let toc = unsafe { toc.assume_init() };

        let song_sectors = (first_track..last_track - 1)
            .map(|i| unsafe {
                (
                    cdio_msf_to_lsn(toc.get(i as usize).unwrap()),
                    cdio_msf_to_lsn(toc.get((i + 1) as usize).unwrap()),
                )
            })
            .collect::<Vec<_>>();

        // Some albus contains a single track only
        let songs = if tracks > 1 {
            vec![
                Song::new(1, song_sectors[0])?,
                Song::new(2, song_sectors[1])?,
            ]
        } else {
            vec![Song::new(1, song_sectors[0])?]
        };

        // Set the number of tracks for this CD
        state.lock().unwrap().total_tracks = tracks;

        Ok(Self {
            cdio,
            song_sectors,
            state,
            tracks,
            songs,
        })
    }

    pub fn get_drive() -> Option<*const i8> {
        let mut driver_id = Box::new(driver_id_t_DRIVER_LINUX);
        let all_cd_drives = unsafe { cdio_get_devices_ret(&mut *driver_id) };
        let cdda_drives = unsafe {
            cdio_get_devices_with_cap(
                all_cd_drives,
                cdio_fs_t_CDIO_FS_AUDIO.try_into().unwrap(),
                0,
            )
        };
        unsafe { cdio_free_device_list(all_cd_drives) };
        if cdda_drives.is_null() || (unsafe { *cdda_drives }).is_null() {
            None
        } else {
            Some(unsafe { *cdda_drives })
        }
    }

    pub fn handle(&mut self) -> Result<()> {
        loop {
            let action = {
                let lock = self.state.lock().unwrap();
                *lock.state_changed.write().unwrap() = false;
                lock.action.clone()
            };

            match action {
                Action::Stop => break,
                Action::Play(track) => {
                    let track = track as usize;
                    // The song to play is different than the current
                    if track != self.songs[0].track_id {
                        // The song to play is the next cached song
                        if track == self.songs[1].track_id {
                            self.songs.remove(0);
                        } else {
                            // Remove both cached songs
                            self.songs.clear();
                            // Load whanever songs we need
                            self.songs
                                .push(Song::new(track, self.song_sectors[track - 1])?);
                        }
                        let next_track_id = track + 1;
                        if next_track_id < self.tracks.into() {
                            // Add the next song to the queue
                            self.songs.push(Song::new(
                                next_track_id,
                                self.song_sectors[next_track_id - 1],
                            )?);
                        }
                    }
                    self.read_cd()?;
                }
                Action::Pause(_) => self.state.lock().unwrap().wait_for_change(),
            }
        }

        Ok(())
    }

    fn read_cd(&mut self) -> Result<()> {
        // The song hasn't been read yet
        let mut ended = self.songs[0].ended;
        if !ended {
            self.songs[0].read(self.cdio, self.state.clone())?;
            ended = self.songs[0].ended;
            // The song has been fully read
            if ended && self.songs.len() == 2 {
                // start reading the next
                self.songs[1].read(self.cdio, self.state.clone())?;
                ended = self.songs[1].ended;
            }
        }
        // Do this after the block above has been evaluated
        if ended {
            // We cached two songs, wait for change
            self.state.lock().unwrap().wait_for_change();
        }

        Ok(())
    }
}
