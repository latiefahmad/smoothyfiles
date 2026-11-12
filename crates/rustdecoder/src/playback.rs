//! Video and audio together, the audio clock as master.
//!
//! ```text
//!   audio feed thread:  AudioDecoder ─► AudioSink::push ─► device ─► audio clock ─┐
//!   decode thread:      Player's ring of pictures                                   ├─► Player::clock
//!   caller:             Playback::frame() = Player::frame_for(clock time)  ◄───────┘
//! ```
//!
//! The [`Player`]'s [`crate::player::Clock`] runs over the sink's [`AudioSink::time_source`], so
//! the movie time is the time of the sound actually played (see [`crate::audio_out`] for how
//! under-runs and the device period are handled). The picture shown is the one
//! [`Player::frame_for`] picks for that time: the latest whose time has come, pictures passed
//! over count as dropped, pictures shown more than a frame late count as late. Without an
//! audio track, without a device, or with audio the file's codec doesn't allow, the player
//! keeps its system clock and nothing else changes.
//!
//! [`Playback::play`] waits until the audio queue holds a little sound before it starts the
//! device, so the stream never starts on an empty buffer. [`Playback::seek`] moves both sides to
//! `t`: the video as in [`Player::seek`], the audio to the packet containing `t` with the
//! decoded sound before `t` dropped, so the first sample queued is the sample at `t`.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::audio::{AudioDecoder, AudioInfo};
use crate::mp4::{AudioTrack, Movie};
use crate::audio_out::{AudioOutput, AudioSink, OutputOptions};
use crate::player::{Player, Shown, Stats, SystemTime, TimeSource};
use crate::Result;

/// Seconds of sound the feed thread keeps queued ahead of the device.
const QUEUE_AHEAD: f64 = 0.4;
/// Seconds that must be queued before the device starts.
const PREROLL: f64 = 0.15;

pub struct PlaybackOptions {
    /// Pictures decoded ahead (see [`Player::open`]).
    pub ring: usize,
    /// Use the audio track and device when there are some.
    pub audio: bool,
    /// Silence, see [`OutputOptions`]. Default: muted.
    pub output: OutputOptions,
}

impl Default for PlaybackOptions {
    fn default() -> Self {
        PlaybackOptions { ring: 6, audio: true, output: OutputOptions::default() }
    }
}

enum Feed {
    Seek(f64, Sender<()>),
    /// Another audio track, started at movie time `t`.
    Switch(Box<AudioDecoder>, f64, Sender<()>),
}

struct Audio {
    sink: Arc<dyn AudioSink>,
    info: AudioInfo,
    ended: Arc<AtomicBool>,
    cmd: Sender<Feed>,
    stop: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

pub struct Playback {
    pub player: Player,
    audio: Option<Audio>,
    playing: bool,
    audio_note: String,
    /// The device output when [`Playback::open`] made it: volume and mute go to it.
    output: Option<Arc<AudioOutput>>,
    path: std::path::PathBuf,
    /// What the file offers ([`Movie::audio_tracks`]) and which entry plays.
    tracks: Vec<AudioTrack>,
    current: Option<usize>,
}

/// The stream a decoder feeds the sink: a group of more than two mono tracks plays its first
/// two as left and right (the rest stay in the decoder), anything else as it is.
fn out_channels(dec: &AudioDecoder) -> (usize, bool) {
    let ch = dec.info().channels as usize;
    if dec.is_group() && ch > 2 { (2, true) } else { (ch, false) }
}

fn feed(mut dec: AudioDecoder, sink: Arc<dyn AudioSink>, rx: std::sync::mpsc::Receiver<Feed>, ended: Arc<AtomicBool>, stop: Arc<AtomicBool>) {
    let info = dec.info().clone();
    let (mut out_ch, mut pair) = out_channels(&dec);
    let mut in_ch = info.channels as usize;
    sink.set_input(info.sample_rate, out_ch);
    let mut done = false;
    while !stop.load(Ordering::Relaxed) {
        // a seek is handled before anything else; the wait below also wakes for it
        let msg = if done || sink.buffered() >= QUEUE_AHEAD {
            match rx.recv_timeout(Duration::from_millis(4)) {
                Ok(m) => Some(m),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        } else {
            rx.try_recv().ok()
        };
        if let Some(Feed::Switch(new, t, ack)) = msg {
            dec = *new;
            (out_ch, pair) = out_channels(&dec);
            in_ch = dec.info().channels as usize;
            sink.set_input(dec.info().sample_rate, out_ch);
            dec.seek(t);
            sink.flush();
            done = false;
            ended.store(false, Ordering::SeqCst);
            let _ = ack.send(());
            continue;
        }
        if let Some(Feed::Seek(t, ack)) = msg {
            dec.seek(t);
            sink.flush();
            done = false;
            ended.store(false, Ordering::SeqCst);
            let _ = ack.send(());
            continue;
        }
        if done || sink.buffered() >= QUEUE_AHEAD {
            continue;
        }
        match dec.next_block() {
            Ok(Some(b)) if pair => {
                let front: Vec<f32> = b.samples.chunks_exact(in_ch).flat_map(|f| [f[0], f[1]]).collect();
                sink.push(b.pts, &front);
            }
            Ok(Some(b)) => sink.push(b.pts, &b.samples),
            Ok(None) => {
                sink.end_of_data();
                done = true;
                ended.store(true, Ordering::SeqCst);
            }
            Err(e) => {
                log::warn!("audio decode stopped: {e}");
                sink.end_of_data();
                done = true;
                ended.store(true, Ordering::SeqCst);
            }
        }
    }
}

impl Playback {
    /// Opens a clip with the default audio device. Whatever keeps the audio from working (no
    /// track, no device, a codec that is not supported) leaves video-only playback on the
    /// system clock; [`Playback::audio_note`] says why.
    pub fn open(path: &Path, opts: PlaybackOptions) -> Result<Playback> {
        let mut note = String::new();
        let mut output = None;
        let sink: Option<Arc<dyn AudioSink>> = if opts.audio {
            match AudioOutput::open(opts.output) {
                Ok(o) => {
                    let o = Arc::new(o);
                    output = Some(o.clone());
                    Some(o)
                }
                Err(e) => {
                    note = format!("no audio output: {e}");
                    None
                }
            }
        } else {
            note = "audio switched off".into();
            None
        };
        let mut p = Self::open_with_sink(path, opts.ring, sink)?;
        p.output = output;
        if p.audio.is_none() && p.audio_note.is_empty() {
            p.audio_note = note;
        }
        Ok(p)
    }

    /// Opens a clip with a given audio sink (a test's fake, or an [`AudioOutput`] set up by the
    /// caller). `None` plays the video alone on the system clock.
    pub fn open_with_sink(path: &Path, ring: usize, sink: Option<Arc<dyn AudioSink>>) -> Result<Playback> {
        let mut note = String::new();
        let mut decoder = None;
        let mut tracks = Vec::new();
        let mut current = None;
        if sink.is_some() {
            match std::fs::File::open(path).map_err(crate::Error::Io).and_then(|mut f| crate::container::read(&mut f)) {
                Ok(movie) => {
                    tracks = movie.audio_tracks();
                    // the default first, then the others in order: the first that opens plays
                    let first = Movie::default_audio_track(&tracks);
                    let order: Vec<usize> = first.into_iter().chain((0..tracks.len()).filter(|&i| Some(i) != first)).collect();
                    for i in order {
                        match AudioDecoder::open_choice(path, &movie, &tracks[i]) {
                            Ok(d) => {
                                decoder = Some(d);
                                current = Some(i);
                                note.clear();
                                break;
                            }
                            Err(e) => note = format!("audio track not playable: {e}"),
                        }
                    }
                    if tracks.is_empty() {
                        note = "no audio track".into();
                    }
                }
                Err(e) => note = format!("audio track not playable: {e}"),
            }
        }
        let time: Arc<dyn TimeSource> = match (&sink, &decoder) {
            (Some(s), Some(_)) => s.time_source(),
            _ => Arc::new(SystemTime::new()),
        };
        let mut player = Player::open_with(path, ring, time)?;
        // the clock starts at whichever comes first: with the sound earlier than the first
        // picture, starting at the picture played the sound that much late
        if let Some(d) = &decoder {
            player.start_earlier(d.first_pts().max(0.0));
        }
        let decoder_ok = decoder.is_some();
        let audio = match (sink, decoder) {
            (Some(sink), Some(dec)) => {
                let mut info = dec.info().clone();
                info.channels = out_channels(&dec).0 as u16;
                let ended = Arc::new(AtomicBool::new(false));
                let stop = Arc::new(AtomicBool::new(false));
                let (tx, rx) = channel();
                let (s2, e2, st2) = (sink.clone(), ended.clone(), stop.clone());
                let thread = std::thread::Builder::new().name("rustdecoder-audio-feed".into()).spawn(move || feed(dec, s2, rx, e2, st2)).map_err(crate::Error::Io)?;
                Some(Audio { sink, info, ended, cmd: tx, stop, thread: Mutex::new(Some(thread)) })
            }
            _ => None,
        };
        Ok(Playback { player, audio, playing: false, audio_note: note, output: None, path: path.to_path_buf(), tracks, current: if decoder_ok { current } else { None } })
    }

    /// Output volume 0..1 (with the device output [`Playback::open`] made; otherwise ignored).
    pub fn set_volume(&self, v: f32) {
        if let Some(o) = &self.output {
            o.set_volume(v);
        }
    }

    /// Silences the output (or not); same condition as [`Playback::set_volume`].
    pub fn set_muted(&self, muted: bool) {
        if let Some(o) = &self.output {
            o.set_muted(muted);
        }
    }

    /// The audio the file offers: tracks (several mono tracks of one recording as one entry).
    pub fn audio_tracks(&self) -> &[AudioTrack] {
        &self.tracks
    }

    /// Which entry of [`Playback::audio_tracks`] is playing.
    pub fn audio_track(&self) -> Option<usize> {
        self.current
    }

    /// Plays entry `i` of [`Playback::audio_tracks`] from now on: the new track starts at the
    /// movie time of the moment (the video is not touched), playing or paused. The track is
    /// opened while the old one keeps playing; the sound is cut over by one flush, so what is
    /// missing is the time to decode the first blocks.
    pub fn set_audio_track(&mut self, i: usize) -> Result<()> {
        if self.audio.is_none() {
            return Err(crate::Error::Unsupported("no audio output to switch"));
        }
        let choice = self.tracks.get(i).ok_or(crate::Error::Invalid("no such audio track"))?;
        if self.current == Some(i) {
            return Ok(());
        }
        let mut f = std::fs::File::open(&self.path)?;
        let movie = crate::container::read(&mut f)?;
        let dec = AudioDecoder::open_choice(&self.path, &movie, choice)?;
        let mut info = dec.info().clone();
        info.channels = out_channels(&dec).0 as u16;
        let was = self.playing;
        self.pause();
        let t = self.player.clock.time();
        let (tx, rx) = channel();
        let Some(a) = self.audio.as_mut() else { return Err(crate::Error::Unsupported("no audio output to switch")) };
        if a.cmd.send(Feed::Switch(Box::new(dec), t, tx)).is_err() {
            return Err(crate::Error::Invalid("audio feed stopped"));
        }
        let _ = rx.recv_timeout(Duration::from_secs(2));
        a.info = info;
        self.current = Some(i);
        if was {
            self.play();
        }
        Ok(())
    }

    /// Why there is no audio, empty when there is.
    pub fn audio_note(&self) -> &str {
        &self.audio_note
    }

    pub fn audio_info(&self) -> Option<&AudioInfo> {
        self.audio.as_ref().map(|a| &a.info)
    }

    pub fn has_audio(&self) -> bool {
        self.audio.is_some()
    }

    /// The whole audio track has been queued (nothing more will come until a seek).
    pub fn audio_ended(&self) -> bool {
        self.audio.as_ref().is_none_or(|a| a.ended.load(Ordering::SeqCst))
    }

    pub fn sink(&self) -> Option<&Arc<dyn AudioSink>> {
        self.audio.as_ref().map(|a| &a.sink)
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    /// Movie time now (the audio clock when there is audio).
    pub fn time(&self) -> f64 {
        self.player.clock.time()
    }

    /// Movie time of the sound being heard, when there is audio.
    pub fn audio_position(&self) -> Option<f64> {
        self.audio.as_ref().and_then(|a| a.sink.position())
    }

    pub fn stats(&self) -> Stats {
        self.player.stats()
    }

    /// Blocks (briefly) until enough sound is queued to start the device cleanly, and the first
    /// pictures are decoded.
    fn preroll(&mut self) {
        let t0 = Instant::now();
        if let Some(a) = &self.audio {
            while a.sink.buffered() < PREROLL && !a.ended.load(Ordering::SeqCst) && t0.elapsed() < Duration::from_secs(2) {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        let now = self.player.clock.time();
        self.player.wait_ahead(now, Duration::from_secs(2));
    }

    /// Starts (or continues) playing from the current time.
    pub fn play(&mut self) {
        if self.playing {
            return;
        }
        self.preroll();
        if let Some(a) = &self.audio {
            a.sink.play();
        }
        self.player.clock.resume();
        self.playing = true;
    }

    pub fn pause(&mut self) {
        if !self.playing {
            return;
        }
        self.player.clock.pause();
        if let Some(a) = &self.audio {
            a.sink.pause();
        }
        self.playing = false;
    }

    /// Moves video and audio to movie time `t`; keeps playing if it was playing.
    pub fn seek(&mut self, t: f64) {
        let was = self.playing;
        self.pause();
        self.player.seek(t);
        if let Some(a) = &self.audio {
            let (tx, rx) = channel();
            if a.cmd.send(Feed::Seek(self.player.clock.time(), tx)).is_ok() {
                let _ = rx.recv_timeout(Duration::from_secs(2));
            }
        }
        if was {
            self.play();
        }
    }

    /// The picture to show now.
    pub fn frame(&mut self) -> Option<Shown> {
        self.player.frame_now()
    }

    /// Every picture and all the sound has been taken.
    pub fn finished(&self) -> bool {
        self.player.finished()
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        if let Some(a) = &self.audio {
            a.stop.store(true, Ordering::SeqCst);
            a.sink.pause();
            if let Some(h) = a.thread.lock().ok().and_then(|mut g| g.take()) {
                let _ = h.join();
            }
        }
    }
}
