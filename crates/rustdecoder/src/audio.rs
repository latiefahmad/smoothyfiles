//! Audio of an MP4 / MOV file as interleaved `f32` PCM.
//!
//! * PCM tracks (`lpcm`, `sowt`, `twos`, `in24`, `in32`, `fl32`, `fl64`, `ipcm`, `fpcm`; 8 / 16 /
//!   24 / 32 bit integer, 32 / 64 bit float, either byte order) are converted here. Integers go
//!   to `x / 2^(bits-1)`, which is exact in `f32` up to 24 bits.
//! * AAC, Opus, MP3, Vorbis (and AC-3 / E-AC-3 where a Dolby decoder is installed) go through
//!   Windows' own decoders, Media Foundation transforms found with `MFTEnumEx`: raw packets in
//!   with the codec's setup data as `MF_MT_USER_DATA`, PCM out ([`AudioMft`], [`MftCodec`]).
//!   This is the "OS decoder at first" of the design document; an own decoder can replace one
//!   without touching the rest.
//! * FLAC is decoded in [`crate::flac`] (the OS transform blocks).
//!
//! Times follow the track's edit list the way [`crate::mp4`] applies it (an AAC encoder's delay
//! is an edit that skips the first 1024 samples, so the first packet starts at a negative
//! time). The decoder drops every frame before time 0 (and before a seek target), so the first
//! sample it returns is the sample at that time.

use std::fs::File;
use std::path::Path;

use crate::flac::FlacDecoder;
use crate::mp4::{AudioTrack, Codec, Movie, Track};
use crate::{Error, Result};

/// Frames per block at most (a PCM chunk can be a second long).
const BLOCK_FRAMES: usize = 4096;

/// What a track's audio is like.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub codec: &'static str,
    /// Seconds from the track's first to its last sample time plus the last packet (approximate
    /// for AAC, exact for PCM).
    pub duration: f64,
}

/// A run of decoded audio: `samples.len() / channels` frames starting at `pts` seconds.
#[derive(Debug, Clone)]
pub struct AudioBlock {
    pub pts: f64,
    pub samples: Vec<f32>,
}

enum Kind_ {
    Flac(Box<FlacDecoder>),
    Pcm { bits: u16, float: bool, big_endian: bool },
    #[cfg(windows)]
    Mft(MftCodec, Box<AudioMft>),
}

/// Mono tracks played as the channels of one stream (see [`AudioTrack::members`]): every member is
/// a decoder of its own track, and blocks are cut so that frame `n` of every channel is the same
/// instant. A member that ends early (or starts late) is silent for the rest (the start).
struct Group {
    members: Vec<Member>,
}

struct Member {
    dec: AudioDecoder,
    /// Decoded mono samples not handed out yet, and the frame index of the first.
    buf: Vec<f32>,
    start: i64,
    ended: bool,
}

impl Member {
    fn fill(&mut self) -> Result<()> {
        if self.buf.is_empty() && !self.ended {
            match self.dec.next_block()? {
                Some(b) => {
                    self.start = (b.pts * self.dec.info.sample_rate as f64).round() as i64;
                    self.buf = b.samples;
                }
                None => self.ended = true,
            }
        }
        Ok(())
    }
}

/// Decodes one audio track of a file (by default the one the container marks, see
/// [`Movie::audio_tracks`]), or a group of mono tracks as one multichannel stream.
pub struct AudioDecoder {
    group: Option<Box<Group>>,
    file: File,
    track: Track,
    info: AudioInfo,
    kind: Kind_,
    /// Next sample (packet / chunk) to read.
    next: usize,
    /// Frames before this are dropped (frame index in the output rate, 0 = time 0).
    skip_frame: i64,
    /// Frame index of the next frame the decoder will output.
    cursor: i64,
    cursor_set: bool,
    drained: bool,
    /// Frame index where the sound ends (Opus `DiscardPadding`), if the container says.
    end_frame: Option<i64>,
    pending: Vec<f32>,
    pending_start: i64,
    pending_off: usize,
    buf: Vec<u8>,
}

impl AudioDecoder {
    /// Opens the default audio of `path` (the container's default track, else the first; several
    /// mono PCM tracks of one recording count as one). `Ok(None)` when the file has none.
    pub fn open(path: &Path) -> Result<Option<Self>> {
        let mut file = File::open(path)?;
        let movie = crate::container::read(&mut file)?;
        let list = movie.audio_tracks();
        let Some(choice) = Movie::default_audio_track(&list) else { return Ok(None) };
        Self::open_choice(path, &movie, &list[choice]).map(Some)
    }

    /// Opens one entry of [`Movie::audio_tracks`] (`movie` read from `path`).
    pub fn open_choice(path: &Path, movie: &Movie, choice: &AudioTrack) -> Result<Self> {
        let first = movie.tracks.get(*choice.members.first().ok_or(Error::Invalid("empty audio choice"))?).ok_or(Error::Invalid("audio choice"))?;
        let mut d = Self::from_track(File::open(path)?, first.clone())?;
        if choice.members.len() > 1 {
            let mut members = Vec::new();
            for &m in &choice.members {
                let t = movie.tracks.get(m).ok_or(Error::Invalid("audio choice"))?;
                members.push(Member { dec: Self::from_track(File::open(path)?, t.clone())?, buf: Vec::new(), start: 0, ended: false });
            }
            d.info.channels = members.len() as u16;
            d.group = Some(Box::new(Group { members }));
            d.seek(0.0);
        }
        Ok(d)
    }

    /// The decoder plays several mono tracks as channels.
    pub fn is_group(&self) -> bool {
        self.group.is_some()
    }

    fn from_track(file: File, track: Track) -> Result<Self> {
        let mut sample_rate = track.sample_rate;
        let channels = track.channels.max(1);
        let mut flac_channels = 0;
        let (kind, codec) = match &track.codec {
            Codec::Pcm { bits, float, big_endian } => {
                if !matches!((*bits, *float), (8 | 16 | 24 | 32, false) | (32 | 64, true)) {
                    return Err(Error::Unsupported("PCM sample size"));
                }
                (Kind_::Pcm { bits: *bits, float: *float, big_endian: *big_endian }, "PCM")
            }
            #[cfg(windows)]
            Codec::Aac { config } => Self::mft(MftCodec::Aac, config, &mut sample_rate, channels)?,
            #[cfg(windows)]
            Codec::Opus { config } => Self::mft(MftCodec::Opus, config, &mut sample_rate, channels)?,
            Codec::Flac { config } => {
                let si = crate::flac::stream_info(config).ok_or(Error::Invalid("FLAC STREAMINFO"))?;
                sample_rate = si.sample_rate;
                flac_channels = si.channels;
                (Kind_::Flac(Box::new(FlacDecoder::new(si)?)), "FLAC")
            }
            #[cfg(windows)]
            Codec::Mp3 => Self::mft(MftCodec::Mp3, &[], &mut sample_rate, channels)?,
            #[cfg(windows)]
            Codec::Other if track.fourcc == *b"mp2 " => Self::mft(MftCodec::Mp2, &[], &mut sample_rate, channels)?,
            #[cfg(windows)]
            Codec::Ac3 { config } => Self::mft(MftCodec::Ac3, config, &mut sample_rate, channels)?,
            #[cfg(windows)]
            Codec::Eac3 { config } => Self::mft(MftCodec::Eac3, config, &mut sample_rate, channels)?,
            #[cfg(windows)]
            Codec::Vorbis { config } => Self::mft(MftCodec::Vorbis, config, &mut sample_rate, channels)?,
            _ => return Err(Error::Unsupported("audio codec other than PCM, AAC, Opus, FLAC, MP3, AC-3, E-AC-3 and Vorbis")),
        };
        if sample_rate == 0 {
            return Err(Error::Invalid("audio sample rate"));
        }
        let channels = match &kind {
            Kind_::Flac(_) => flac_channels,
            #[cfg(windows)]
            Kind_::Mft(_, m) => m.channels,
            _ => channels,
        };
        let duration = {
            let last = track.samples.last();
            match (&kind, last) {
                // an AES3 element's size is not its sample count: the step to the one before is
                (Kind_::Pcm { .. }, Some(l)) if track.fourcc == crate::mp4::AES3 => {
                    let step = track.samples.len().checked_sub(2).map_or(0, |i| l.pts - track.samples[i].pts);
                    (l.pts.max(0) + step) as f64 / track.timescale.max(1) as f64
                }
                (Kind_::Pcm { bits, .. }, Some(l)) => {
                    let fb = (*bits as u64 / 8) * channels as u64;
                    (l.pts.max(0) as f64 + (l.size as u64 / fb.max(1)) as f64) / track.timescale.max(1) as f64
                }
                (_, Some(l)) => track.seconds(l.pts) + 1024.0 / sample_rate as f64,
                _ => 0.0,
            }
        };
        let info = AudioInfo { sample_rate, channels, codec, duration };
        let mut d = AudioDecoder {
            group: None,
            file,
            track,
            info,
            kind,
            next: 0,
            skip_frame: 0,
            cursor: 0,
            cursor_set: false,
            drained: false,
            end_frame: None,
            pending: Vec::new(),
            pending_start: 0,
            pending_off: 0,
            buf: Vec::new(),
        };
        d.end_frame = d.opus_end();
        d.seek(0.0);
        Ok(d)
    }

    #[cfg(windows)]
    fn mft(codec: MftCodec, config: &[u8], rate: &mut u32, channels: u16) -> Result<(Kind_, &'static str)> {
        let m = AudioMft::new(codec, config, *rate, channels)?;
        *rate = m.rate;
        Ok((Kind_::Mft(codec, Box::new(m)), codec.name()))
    }

    /// Opus: where the sound ends once `DiscardPadding` is taken off the last packet. Packets
    /// are taken to last as long as the first one (the encoders make them so), the last by its
    /// own table of contents.
    fn opus_end(&mut self) -> Option<i64> {
        let Kind_::Mft(MftCodec::Opus, _) = self.kind else { return None };
        if self.track.discard_padding_ns == 0 || self.track.samples.len() < 2 {
            return None;
        }
        let n = self.track.samples.len();
        let mut buf = Vec::new();
        self.track.read_sample(&mut self.file, 0, &mut buf).ok()?;
        let first = opus_packet_frames(&buf)?;
        self.track.read_sample(&mut self.file, n - 1, &mut buf).ok()?;
        let last = opus_packet_frames(&buf)?;
        let rate = self.info.sample_rate as i128;
        let delay = (self.track.codec_delay_ns as i128 * rate + 500_000_000) / 1_000_000_000;
        let pad = (self.track.discard_padding_ns as i128 * rate + 500_000_000) / 1_000_000_000;
        Some(((n as i128 - 1) * first as i128 + last as i128 - delay - pad) as i64)
    }

    pub fn info(&self) -> &AudioInfo {
        &self.info
    }

    pub fn track(&self) -> &Track {
        &self.track
    }

    /// Time of the first sample in seconds (before trimming to 0 for AAC this is negative).
    pub fn first_pts(&self) -> f64 {
        self.track.samples.first().map_or(0.0, |s| self.track.seconds(s.pts))
    }

    /// Moves to time `t` seconds: the next block starts at the frame at `t` (or at the first
    /// frame when `t` is before the start). AAC restarts two packets earlier to prime the
    /// decoder; what it outputs before `t` is dropped.
    pub fn seek(&mut self, t: f64) {
        if let Some(g) = &mut self.group {
            for m in &mut g.members {
                m.dec.seek(t);
                m.buf.clear();
                m.ended = false;
            }
            return;
        }
        let t = t.max(0.0);
        let rate = self.info.sample_rate as f64;
        self.skip_frame = (t * rate).round() as i64;
        let ts = self.track.timescale.max(1) as f64;
        let target = (t * ts).floor() as i64;
        // last packet whose time is not after the target
        let idx = self.track.samples.partition_point(|s| s.pts <= target).saturating_sub(1);
        self.next = match self.kind {
            Kind_::Pcm { .. } | Kind_::Flac(_) => idx,
            #[cfg(windows)]
            Kind_::Mft(c, _) => idx.saturating_sub(c.preroll()),
        };
        self.pending.clear();
        self.pending_off = 0;
        self.cursor_set = false;
        self.drained = false;
        #[cfg(windows)]
        if let Kind_::Mft(_, m) = &mut self.kind {
            m.flush();
        }
    }

    /// The next block of audio, or `None` at the end.
    pub fn next_block(&mut self) -> Result<Option<AudioBlock>> {
        if self.group.is_some() {
            return self.group_block();
        }
        let ch = self.info.channels as usize;
        loop {
            let have = (self.pending.len() / ch).saturating_sub(self.pending_off);
            if have == 0 {
                if !self.refill()? {
                    return Ok(None);
                }
                continue;
            }
            let start = self.pending_start + self.pending_off as i64;
            let mut n = have.min(BLOCK_FRAMES);
            if let Some(e) = self.end_frame {
                if start >= e {
                    return Ok(None);
                }
                n = n.min((e - start) as usize);
            }
            if start + (n as i64) <= self.skip_frame {
                self.pending_off += n;
                continue;
            }
            let cut = (self.skip_frame - start).max(0) as usize;
            let from = self.pending_off + cut;
            let n = n - cut;
            let samples = self.pending[from * ch..(from + n) * ch].to_vec();
            self.pending_off = from + n;
            return Ok(Some(AudioBlock { pts: (start + cut as i64) as f64 / self.info.sample_rate as f64, samples }));
        }
    }

    /// The next block of a group: up to [`BLOCK_FRAMES`] frames of all members, interleaved.
    fn group_block(&mut self) -> Result<Option<AudioBlock>> {
        let Some(g) = &mut self.group else { return Ok(None) };
        for m in &mut g.members {
            m.fill()?;
        }
        let Some(at) = g.members.iter().filter(|m| !m.buf.is_empty()).map(|m| m.start).min() else { return Ok(None) };
        let mut n = BLOCK_FRAMES;
        for m in g.members.iter().filter(|m| !m.buf.is_empty()) {
            n = n.min(if m.start > at { (m.start - at) as usize } else { m.buf.len() });
        }
        let ch = g.members.len();
        let mut samples = vec![0.0f32; n * ch];
        for (c, m) in g.members.iter_mut().enumerate() {
            if m.buf.is_empty() || m.start > at {
                continue;
            }
            for (f, v) in m.buf[..n].iter().enumerate() {
                samples[f * ch + c] = *v;
            }
            m.buf.drain(..n);
            m.start += n as i64;
        }
        Ok(Some(AudioBlock { pts: at as f64 / self.info.sample_rate as f64, samples }))
    }

    /// Everything from the current position as one vector, with the time of its first frame.
    pub fn decode_all(&mut self) -> Result<(f64, Vec<f32>)> {
        let mut first = None;
        let mut out = Vec::new();
        while let Some(b) = self.next_block()? {
            first.get_or_insert(b.pts);
            out.extend_from_slice(&b.samples);
        }
        Ok((first.unwrap_or(0.0), out))
    }

    /// Fills `pending` with the next decoded run; `false` at the end.
    fn refill(&mut self) -> Result<bool> {
        self.pending.clear();
        self.pending_off = 0;
        let rate = self.info.sample_rate as i128;
        let ts = self.track.timescale.max(1) as i128;
        match &mut self.kind {
            Kind_::Pcm { bits, float, big_endian } => {
                let Some(s) = self.track.samples.get(self.next).copied() else { return Ok(false) };
                self.track.read_sample(&mut self.file, self.next, &mut self.buf)?;
                self.next += 1;
                let derived = ((s.pts as i128 * rate + ts / 2).div_euclid(ts)) as i64;
                // blocks follow each other; a time rounded to the container's tick (1 ms in
                // Matroska) must not open a gap or an overlap
                let slack = (rate / ts) as i64 + 1;
                self.pending_start = if self.cursor_set && (derived - self.cursor).abs() <= slack { self.cursor } else { derived };
                self.cursor_set = true;
                convert_pcm(&self.buf, *bits, *float, *big_endian, &mut self.pending);
                let ch = self.info.channels as usize;
                self.pending.truncate(self.pending.len() / ch * ch);
                self.cursor = self.pending_start + (self.pending.len() / ch) as i64;
                Ok(true)
            }
            Kind_::Flac(f) => {
                let Some(s) = self.track.samples.get(self.next).copied() else { return Ok(false) };
                self.track.read_sample(&mut self.file, self.next, &mut self.buf)?;
                self.next += 1;
                let derived = ((s.pts as i128 * rate + ts / 2).div_euclid(ts)) as i64;
                let slack = (rate / ts) as i64 + 1;
                self.pending_start = if self.cursor_set && (derived - self.cursor).abs() <= slack { self.cursor } else { derived };
                self.cursor_set = true;
                let n = f.decode(&self.buf, &mut self.pending)?;
                self.cursor = self.pending_start + n as i64;
                Ok(true)
            }
            #[cfg(windows)]
            Kind_::Mft(_, m) => loop {
                if let Some(pcm) = m.pull()? {
                    self.pending_start = self.cursor;
                    self.cursor += (pcm.len() / self.info.channels as usize) as i64;
                    self.pending = pcm;
                    return Ok(true);
                }
                if self.drained {
                    return Ok(false);
                }
                let Some(s) = self.track.samples.get(self.next).copied() else {
                    m.drain();
                    self.drained = true;
                    continue;
                };
                self.track.read_sample(&mut self.file, self.next, &mut self.buf)?;
                if !self.cursor_set {
                    self.cursor = ((s.pts as i128 * rate + ts / 2).div_euclid(ts)) as i64;
                    if self.next == 0 && self.track.codec_delay_ns > 0 {
                        // Matroska: the first packet starts CodecDelay before 0, exactly
                        self.cursor = -((self.track.codec_delay_ns as i128 * rate + 500_000_000) / 1_000_000_000) as i64;
                    }
                    self.cursor_set = true;
                }
                // time and duration in 100 ns; the duration is the gap to the next packet
                let tick = |v: i64| (v as i128 * 10_000_000 / ts) as i64;
                let at = self.next;
                let dur = match (self.track.samples.get(at + 1), at.checked_sub(1).and_then(|p| self.track.samples.get(p))) {
                    (Some(n), _) => n.pts - s.pts,
                    (None, Some(p)) => s.pts - p.pts,
                    _ => 0,
                };
                self.next += 1;
                m.push(&self.buf, tick(s.pts), tick(dur.max(0)))?;
            },
        }
    }
}

/// Samples (48 kHz) in an Opus packet, from its table of contents (RFC 6716, section 3.1).
fn opus_packet_frames(p: &[u8]) -> Option<usize> {
    let toc = *p.first()?;
    let config = toc >> 3;
    // frame length in units of 2.5 ms
    let unit = match config {
        0..=11 => [4, 8, 16, 24][(config % 4) as usize],
        12..=15 => [4, 8][(config % 2) as usize],
        _ => [1, 2, 4, 8][(config % 4) as usize],
    };
    let count = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => (*p.get(1)? & 0x3f) as usize,
    };
    Some(count * unit * 120)
}

/// Raw PCM bytes to `f32`, appended to `out`.
pub fn convert_pcm(b: &[u8], bits: u16, float: bool, big: bool, out: &mut Vec<f32>) {
    match (bits, float) {
        (8, false) => out.extend(b.iter().map(|&x| x as i8 as f32 / 128.0)),
        (16, false) => out.extend(b.chunks_exact(2).map(|c| {
            let v = if big { i16::from_be_bytes([c[0], c[1]]) } else { i16::from_le_bytes([c[0], c[1]]) };
            v as f32 / 32768.0
        })),
        (24, false) => out.extend(b.chunks_exact(3).map(|c| {
            let v = if big { (c[0] as i32) << 16 | (c[1] as i32) << 8 | c[2] as i32 } else { (c[2] as i32) << 16 | (c[1] as i32) << 8 | c[0] as i32 };
            ((v << 8) >> 8) as f32 / 8_388_608.0
        })),
        (32, false) => out.extend(b.chunks_exact(4).map(|c| {
            let a = [c[0], c[1], c[2], c[3]];
            let v = if big { i32::from_be_bytes(a) } else { i32::from_le_bytes(a) };
            (v as f64 / 2_147_483_648.0) as f32
        })),
        (32, true) => out.extend(b.chunks_exact(4).map(|c| {
            let a = [c[0], c[1], c[2], c[3]];
            if big { f32::from_be_bytes(a) } else { f32::from_le_bytes(a) }
        })),
        (64, true) => out.extend(b.chunks_exact(8).map(|c| {
            let a: [u8; 8] = c.try_into().unwrap();
            (if big { f64::from_be_bytes(a) } else { f64::from_le_bytes(a) }) as f32
        })),
        _ => {}
    }
}
// ---------------------------------------------------------------------------------------------
// Codecs decoded by the OS (Media Foundation audio decoder transforms)

/// A codec that Windows' own decoders handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MftCodec {
    Aac,
    Opus,
    Mp3,
    /// MPEG-1 audio layer I and II (`MFAudioFormat_MPEG`).
    Mp2,
    Ac3,
    Eac3,
    Vorbis,
}

impl MftCodec {
    pub fn name(self) -> &'static str {
        match self {
            MftCodec::Aac => "AAC",
            MftCodec::Opus => "Opus",
            MftCodec::Mp3 => "MP3",
            MftCodec::Mp2 => "MP2",
            MftCodec::Ac3 => "AC-3",
            MftCodec::Eac3 => "E-AC-3",
            MftCodec::Vorbis => "Vorbis",
        }
    }

    /// Packets decoded before a seek target so the decoder's overlap / bit reservoir is
    /// primed; what they output before the target is dropped.
    fn preroll(self) -> usize {
        match self {
            MftCodec::Aac | MftCodec::Ac3 | MftCodec::Eac3 => 2,
            MftCodec::Mp3 | MftCodec::Mp2 => 4,
            // 80 ms of 20 ms packets, what RFC 7845 asks for
            MftCodec::Opus => 4,
            MftCodec::Vorbis => 2,
        }
    }

    /// Said when no decoder for the codec is installed.
    pub fn missing(self) -> &'static str {
        match self {
            MftCodec::Aac => "AAC audio: no decoder installed (the AAC decoder of Windows is missing)",
            MftCodec::Opus => "Opus audio: no decoder installed (Windows 10 1809 and later have one)",
            MftCodec::Mp3 => "MP3 audio: no decoder installed",
            MftCodec::Mp2 => "MP2 audio: no decoder installed",
            MftCodec::Ac3 => "AC-3 audio: no decoder installed (Windows has only a passthrough converter, no Dolby Digital decoder)",
            MftCodec::Eac3 => "E-AC-3 audio: no decoder installed (Windows has only a passthrough converter, no Dolby Digital Plus decoder)",
            MftCodec::Vorbis => "Vorbis audio: no decoder installed (the Web Media Extensions add one)",
        }
    }

    const ALL: [MftCodec; 7] = [MftCodec::Aac, MftCodec::Opus, MftCodec::Mp3, MftCodec::Mp2, MftCodec::Ac3, MftCodec::Eac3, MftCodec::Vorbis];
}

/// For each codec, the audio decoders this machine offers (their friendly names).
#[cfg(windows)]
pub fn installed_decoders() -> Vec<(&'static str, Vec<String>)> {
    MftCodec::ALL.iter().map(|c| (c.name(), mft::installed(*c))).collect()
}

#[cfg(windows)]
pub use mft::AudioMft;

#[cfg(windows)]
mod mft {
    use std::sync::Once;

    use windows::Win32::Media::MediaFoundation::*;
    use windows::Win32::System::Com::*;
    use windows::core::GUID;

    use super::MftCodec;
    use crate::{Error, Result};

    /// `CLSID_CMSAACDecMFT`
    const CLSID_AAC_DEC: GUID = GUID::from_u128(0x32d186a7_218f_4c75_8876_dd77273a8999);

    fn gpu_err(what: &str, e: windows::core::Error) -> Error {
        Error::Gpu(format!("audio decoder: {what}: {e}"))
    }

    static START: Once = Once::new();

    /// COM (multithreaded, on this thread) and Media Foundation.
    fn startup() {
        START.call_once(|| unsafe {
            let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);
        });
        // an already initialised apartment of another kind is fine
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
    }

    fn subtype(c: MftCodec) -> GUID {
        match c {
            MftCodec::Aac => MFAudioFormat_AAC,
            MftCodec::Opus => MFAudioFormat_Opus,
            MftCodec::Mp3 => MFAudioFormat_MP3,
            MftCodec::Mp2 => MFAudioFormat_MPEG,
            MftCodec::Ac3 => MFAudioFormat_Dolby_AC3,
            MftCodec::Eac3 => MFAudioFormat_Dolby_DDPlus,
            MftCodec::Vorbis => MFAudioFormat_Vorbis,
        }
    }

    /// Every decoder transform registered for the codec, best first.
    fn activates(c: MftCodec) -> Vec<IMFActivate> {
        startup();
        let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Audio, guidSubtype: subtype(c) };
        let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut n = 0u32;
        let flags = MFT_ENUM_FLAG(MFT_ENUM_FLAG_SYNCMFT.0 | MFT_ENUM_FLAG_LOCALMFT.0 | MFT_ENUM_FLAG_SORTANDFILTER.0);
        let mut out = Vec::new();
        unsafe {
            if MFTEnumEx(MFT_CATEGORY_AUDIO_DECODER, flags, Some(&input), None, &mut list, &mut n).is_ok() && !list.is_null() {
                for i in 0..n as usize {
                    // take ownership of each element so it is released
                    if let Some(a) = std::ptr::read(list.add(i)) {
                        out.push(a);
                    }
                }
                CoTaskMemFree(Some(list as *const _));
            }
        }
        out
    }

    fn friendly_name(a: &IMFActivate) -> String {
        unsafe {
            let mut p = windows::core::PWSTR::null();
            let mut len = 0u32;
            if a.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut p, &mut len).is_ok() && !p.is_null() {
                let s = String::from_utf16_lossy(std::slice::from_raw_parts(p.0, len as usize));
                CoTaskMemFree(Some(p.0 as *const _));
                s
            } else {
                "(unnamed)".into()
            }
        }
    }

    pub fn installed(c: MftCodec) -> Vec<String> {
        if c == MftCodec::Aac {
            // found by its class id
            let t: windows::core::Result<IMFTransform> = unsafe {
                startup();
                CoCreateInstance(&CLSID_AAC_DEC, None, CLSCTX_INPROC_SERVER)
            };
            return if t.is_ok() { vec!["AAC Decoder MFT".into()] } else { Vec::new() };
        }
        activates(c).iter().map(friendly_name).collect()
    }

    /// The input media types worth trying for a codec, as `MF_MT_USER_DATA` variants.
    fn user_data_variants(c: MftCodec, config: &[u8]) -> Vec<Vec<u8>> {
        match c {
            MftCodec::Aac => {
                // HEAACWAVEINFO's tail (payload type, profile level, structure type, 2 reserved
                // words), then the AudioSpecificConfig
                let mut user = vec![0u8; 12];
                user[2..4].copy_from_slice(&0x29u16.to_le_bytes());
                user.extend_from_slice(config);
                vec![user]
            }
            MftCodec::Opus => {
                // the transform would cut the pre-skip itself; the decoder does that by time
                // (so a seek can restart anywhere), hence it gets a head with no pre-skip
                let mut head = config.to_vec();
                if head.len() >= 12 && head.starts_with(b"OpusHead") {
                    head[10] = 0;
                    head[11] = 0;
                }
                vec![head, Vec::new()]
            }
            MftCodec::Mp3 => {
                // MPEGLAYER3WAVEFORMAT's tail: wID 1 (MPEGLAYER3_ID_MPEG), flags 2, block size,
                // frames per block 1, codec delay 0
                let mut user = Vec::new();
                user.extend_from_slice(&1u16.to_le_bytes());
                user.extend_from_slice(&2u32.to_le_bytes());
                user.extend_from_slice(&0u16.to_le_bytes());
                user.extend_from_slice(&1u16.to_le_bytes());
                user.extend_from_slice(&0u16.to_le_bytes());
                vec![user, Vec::new()]
            }
            MftCodec::Mp2 => {
                // MPEG1WAVEFORMAT's tail: layer 2, bit rate 0, mode, extension, emphasis, flags
                // (ACM_MPEG_ID_MPEG1 = 0x10), two zero PTS words
                let mut user = Vec::new();
                user.extend_from_slice(&2u16.to_le_bytes());
                user.extend_from_slice(&0u32.to_le_bytes());
                user.extend_from_slice(&1u16.to_le_bytes());
                user.extend_from_slice(&0u16.to_le_bytes());
                user.extend_from_slice(&1u16.to_le_bytes());
                user.extend_from_slice(&0x10u16.to_le_bytes());
                user.extend_from_slice(&[0u8; 8]);
                vec![user, Vec::new()]
            }
            MftCodec::Ac3 | MftCodec::Eac3 => vec![Vec::new()],
            MftCodec::Vorbis => vec![config.to_vec(), Vec::new()],
        }
    }

    /// The best output type the decoder offers: float, else 16 / 24 / 32-bit integers
    /// (the widest). Returns the type, its bits per sample and whether it is float.
    fn pick_output(t: &IMFTransform) -> Option<(IMFMediaType, u32, bool)> {
        let mut best: Option<(IMFMediaType, u32, bool)> = None;
        let mut i = 0;
        while let Ok(o) = unsafe { t.GetOutputAvailableType(0, i) } {
            i += 1;
            let bits = unsafe { o.GetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE) }.unwrap_or(16);
            match unsafe { o.GetGUID(&MF_MT_SUBTYPE) } {
                Ok(g) if g == MFAudioFormat_Float && bits == 32 => return Some((o, 32, true)),
                Ok(g) if g == MFAudioFormat_PCM && matches!(bits, 16 | 24 | 32) && best.as_ref().is_none_or(|b| bits > b.1) => best = Some((o, bits, false)),
                _ => {}
            }
        }
        best
    }

    /// An audio decoder transform: packets in, interleaved `f32` out.
    pub struct AudioMft {
        t: IMFTransform,
        out_sample: IMFSample,
        provides: bool,
        pub rate: u32,
        pub channels: u16,
        bits: u32,
        float: bool,
        pub name: String,
    }

    // SAFETY: the transform is free threaded (both-threaded COM class) and is only ever used
    // from one thread at a time: the decoder owning it moves between threads, never shares.
    unsafe impl Send for AudioMft {}

    impl AudioMft {
        /// `config` is the codec's setup data in the form [`crate::mp4::Codec`] keeps it
        /// (AudioSpecificConfig, OpusHead, ...); `rate` and `channels`
        /// come from the sample entry.
        pub fn new(codec: MftCodec, config: &[u8], rate: u32, channels: u16) -> Result<Self> {
            startup();
            let mut cands: Vec<(IMFTransform, String)> = Vec::new();
            if codec == MftCodec::Aac {
                if let Ok(t) = unsafe { CoCreateInstance::<_, IMFTransform>(&CLSID_AAC_DEC, None, CLSCTX_INPROC_SERVER) } {
                    cands.push((t, "AAC Decoder MFT".into()));
                }
            } else {
                for a in activates(codec) {
                    if let Ok(t) = unsafe { a.ActivateObject::<IMFTransform>() } {
                        cands.push((t, friendly_name(&a)));
                    }
                }
            }
            if cands.is_empty() {
                return Err(Error::Unsupported(codec.missing()));
            }
            let mut last = None;
            for (t, name) in cands {
                match Self::start(t, name, codec, config, rate, channels) {
                    Ok(m) => return Ok(m),
                    Err(e) => last = Some(e),
                }
            }
            Err(last.unwrap_or(Error::Unsupported(codec.missing())))
        }

        fn start(t: IMFTransform, name: String, codec: MftCodec, config: &[u8], rate: u32, channels: u16) -> Result<Self> {
            let bits = 16u32;
            unsafe {
                let e = |w: &'static str| move |e| gpu_err(w, e);
                let mut set = false;
                let mut last_err = None;
                for user in user_data_variants(codec, config) {
                    let ty = MFCreateMediaType().map_err(e("media type"))?;
                    ty.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).map_err(e("major"))?;
                    ty.SetGUID(&MF_MT_SUBTYPE, &subtype(codec)).map_err(e("subtype"))?;
                    ty.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate).map_err(e("rate"))?;
                    ty.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels as u32).map_err(e("channels"))?;
                    ty.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, bits).map_err(e("bits"))?;
                    if codec == MftCodec::Aac {
                        ty.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0).map_err(e("payload"))?;
                        ty.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29).map_err(e("profile"))?;
                    } else {
                        ty.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 1).map_err(e("alignment"))?;
                    }
                    if !user.is_empty() {
                        ty.SetBlob(&MF_MT_USER_DATA, &user).map_err(e("user data"))?;
                    }
                    match t.SetInputType(0, &ty, 0) {
                        Ok(()) => {
                            set = true;
                            break;
                        }
                        Err(er) => last_err = Some(er),
                    }
                }
                if !set {
                    return Err(gpu_err(&format!("{} input type ({name})", codec.name()), last_err.unwrap()));
                }
                let (o, obits, float) = pick_output(&t).ok_or(Error::Unsupported(codec.missing()))?;
                t.SetOutputType(0, &o, 0).map_err(e("output type"))?;
                let out_rate = o.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND).unwrap_or(rate);
                let out_ch = o.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS).unwrap_or(channels as u32);
                t.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).map_err(e("begin"))?;
                t.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).map_err(e("start"))?;
                let info = t.GetOutputStreamInfo(0).map_err(e("stream info"))?;
                let provides = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
                let out_cap = info.cbSize.max(1 << 16);
                let out_sample = MFCreateSample().map_err(e("sample"))?;
                let buf = MFCreateMemoryBuffer(out_cap).map_err(e("buffer"))?;
                out_sample.AddBuffer(&buf).map_err(e("add buffer"))?;
                Ok(AudioMft { t, out_sample, provides, rate: out_rate, channels: out_ch as u16, bits: obits, float, name })
            }
        }

        /// Gives the decoder one packet.
        pub fn push(&mut self, packet: &[u8], time: i64, duration: i64) -> Result<()> {
            unsafe {
                let buf = MFCreateMemoryBuffer(packet.len().max(1) as u32).map_err(|e| gpu_err("buffer", e))?;
                let mut p = std::ptr::null_mut();
                buf.Lock(&mut p, None, None).map_err(|e| gpu_err("lock", e))?;
                std::ptr::copy_nonoverlapping(packet.as_ptr(), p, packet.len());
                buf.Unlock().map_err(|e| gpu_err("unlock", e))?;
                buf.SetCurrentLength(packet.len() as u32).map_err(|e| gpu_err("length", e))?;
                let s = MFCreateSample().map_err(|e| gpu_err("sample", e))?;
                s.AddBuffer(&buf).map_err(|e| gpu_err("add buffer", e))?;
                // some transforms refuse a sample that has no duration
                let _ = s.SetSampleTime(time);
                let _ = s.SetSampleDuration(duration);
                self.t.ProcessInput(0, &s, 0).map_err(|e| gpu_err("input", e))
            }
        }

        /// The decoder will get no more packets: let it release what it holds.
        pub fn drain(&mut self) {
            unsafe {
                let _ = self.t.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0);
            }
        }

        pub fn flush(&mut self) {
            unsafe {
                let _ = self.t.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
                let _ = self.t.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
            }
        }

        /// One run of decoded audio, interleaved `f32`; `None` when it wants more input.
        pub fn pull(&mut self) -> Result<Option<Vec<f32>>> {
            for _ in 0..4 {
                unsafe {
                    if !self.provides {
                        if let Ok(b) = self.out_sample.GetBufferByIndex(0) {
                            let _ = b.SetCurrentLength(0);
                        }
                    }
                    let given = if self.provides { None } else { Some(self.out_sample.clone()) };
                    let mut out = [MFT_OUTPUT_DATA_BUFFER { dwStreamID: 0, pSample: std::mem::ManuallyDrop::new(given), dwStatus: 0, pEvents: std::mem::ManuallyDrop::new(None) }];
                    let mut status = 0u32;
                    let r = self.t.ProcessOutput(0, &mut out, &mut status);
                    // take the references back so they are released
                    let [o] = out;
                    let got = std::mem::ManuallyDrop::into_inner(o.pSample);
                    drop(std::mem::ManuallyDrop::into_inner(o.pEvents));
                    match r {
                        Ok(()) => {
                            let sample = if self.provides { got.ok_or(Error::Invalid("audio decoder gave no sample"))? } else { self.out_sample.clone() };
                            let buf = sample.ConvertToContiguousBuffer().map_err(|e| gpu_err("output buffer", e))?;
                            let (mut p, mut len) = (std::ptr::null_mut(), 0u32);
                            buf.Lock(&mut p, None, Some(&mut len)).map_err(|e| gpu_err("lock output", e))?;
                            let bytes = std::slice::from_raw_parts(p, len as usize).to_vec();
                            buf.Unlock().map_err(|e| gpu_err("unlock output", e))?;
                            let mut pcm = Vec::with_capacity(bytes.len() / 4);
                            super::convert_pcm(&bytes, self.bits as u16, self.float, false, &mut pcm);
                            if pcm.is_empty() {
                                continue;
                            }
                            return Ok(Some(pcm));
                        }
                        Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                        Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                            let Some((c, bits, float)) = pick_output(&self.t) else {
                                return Err(Error::Unsupported("audio decoder offers no PCM output"));
                            };
                            self.t.SetOutputType(0, &c, 0).map_err(|e| gpu_err("output type", e))?;
                            self.rate = c.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND).unwrap_or(self.rate);
                            self.channels = c.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS).unwrap_or(self.channels as u32) as u16;
                            self.bits = bits;
                            self.float = float;
                        }
                        Err(e) => return Err(gpu_err("output", e)),
                    }
                }
            }
            Ok(None)
        }
    }
}
