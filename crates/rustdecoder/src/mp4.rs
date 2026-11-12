//! ISO base media files: MP4, MOV, M4V, M4A, 3GP.
//!
//! [`read`] walks the top-level boxes (skipping `mdat` without reading it), loads `moov` and turns
//! every track's sample tables into one flat list: where each sample is, how big, when it is
//! decoded and shown, and whether decoding can start there. Times follow the first edit of the
//! edit list the way players do (a B-frame stream's first picture is shown at 0, its first
//! decode time is negative). Nothing from the file is trusted: every table is bounded by the box
//! that holds it, and tables that disagree are cut to the shortest.
//!
//! Fragmented files: `mvex` / `trex` give the defaults, and every `moof` (`mfhd`, `traf` with
//! `tfhd`, `tfdt`, `trun`) appends its samples to the track's table after the ones `moov`
//! lists (none for `empty_moov`), so everything downstream sees one flat table. Only box headers
//! and the `moof` boxes themselves are read, never `mdat`; `sidx` / `mfra` are not needed because
//! every `moof` is walked. A last fragment that was cut keeps its complete samples.

use std::io::{Read, Seek, SeekFrom};

use crate::{Error, Result};

/// `moov` larger than this is refused (a two-hour film's is a few MB).
const MOOV_MAX: u64 = 256 << 20;
/// Samples per track at most (a day of 240 fps video is 21 million).
const SAMPLES_MAX: usize = 50_000_000;

/// The whole file: movie timescale and the tracks.
#[derive(Debug, Clone)]
pub struct Movie {
    /// Units per second of [`Movie::duration`].
    pub timescale: u32,
    pub duration: u64,
    pub tracks: Vec<Track>,
}

impl Movie {
    pub fn duration_secs(&self) -> f64 {
        if self.timescale == 0 { 0.0 } else { self.duration as f64 / self.timescale as f64 }
    }

    /// The first video track.
    pub fn video(&self) -> Option<&Track> {
        let mut v = self.tracks.iter().filter(|t| t.kind == Kind::Video && !t.samples.is_empty());
        let first = v.next()?;
        if first.default { Some(first) } else { std::iter::once(first).chain(v).find(|t| t.default).or(Some(first)) }
    }

    /// The audio a player offers: one entry per playable audio track, except that several mono
    /// PCM tracks that belong together (see [`AudioTrack::members`]) are one multichannel entry.
    /// In file order, the entry of a group at the place of its first track.
    pub fn audio_tracks(&self) -> Vec<AudioTrack> {
        let usable: Vec<usize> = (0..self.tracks.len()).filter(|&i| self.tracks[i].kind == Kind::Audio && !self.tracks[i].samples.is_empty()).collect();
        let mut out: Vec<AudioTrack> = Vec::new();
        let mut taken = vec![false; self.tracks.len()];
        for &i in &usable {
            if taken[i] {
                continue;
            }
            let t = &self.tracks[i];
            let mut members = vec![i];
            if t.channels == 1 && matches!(t.codec, Codec::Pcm { .. }) {
                for &j in usable.iter().filter(|&&j| j > i && !taken[j]) {
                    if self.mono_pair(i, j) {
                        members.push(j);
                    }
                }
            }
            // a lone mono track stays a track; two or more are a group
            for &m in &members {
                taken[m] = true;
            }
            out.push(AudioTrack {
                channels: members.len() as u16 * t.channels.max(1),
                codec: t.codec.audio_name(),
                sample_rate: t.sample_rate,
                language: t.meta.language.clone(),
                name: t.meta.name.clone(),
                default: members.iter().any(|&m| self.tracks[m].default),
                members,
            });
        }
        out
    }

    /// Which entry of [`Movie::audio_tracks`] to play first: the first the container marks as
    /// default, else the first.
    pub fn default_audio_track(list: &[AudioTrack]) -> Option<usize> {
        list.iter().position(|a| a.default).or(if list.is_empty() { None } else { Some(0) })
    }

    /// Mono PCM tracks `a` and `b` of the same recording: same format and rate, the same length
    /// (within a millisecond), and no two different languages.
    fn mono_pair(&self, a: usize, b: usize) -> bool {
        let (x, y) = (&self.tracks[a], &self.tracks[b]);
        let same_lang = |p: &str, q: &str| p == q || p.is_empty() || q.is_empty() || p == "und" || q == "und";
        let end = |t: &Track| t.samples.last().map_or(0.0, |s| t.seconds(s.pts));
        x.channels == 1
            && y.channels == 1
            && x.codec == y.codec
            && x.fourcc == y.fourcc
            && x.sample_rate == y.sample_rate
            && x.samples.len() == y.samples.len()
            && (end(x) - end(y)).abs() < 0.001
            && same_lang(&x.meta.language, &y.meta.language)
    }

    /// The first audio track.
    pub fn audio(&self) -> Option<&Track> {
        let mut v = self.tracks.iter().filter(|t| t.kind == Kind::Audio && !t.samples.is_empty());
        let first = v.next()?;
        if first.default { Some(first) } else { std::iter::once(first).chain(v).find(|t| t.default).or(Some(first)) }
    }
}

/// One audio choice of a file (see [`Movie::audio_tracks`]).
#[derive(Debug, Clone, PartialEq)]
pub struct AudioTrack {
    /// Indices into [`Movie::tracks`]: one track, or the mono tracks of a group (channel 1 =
    /// the first, channel 2 = the second, and so on).
    pub members: Vec<usize>,
    pub codec: &'static str,
    /// Channels of the choice (the number of members for a group).
    pub channels: u16,
    pub sample_rate: u32,
    /// ISO 639-2 (`eng`) or a BCP 47 tag (`en-US`); empty when the file does not say.
    pub language: String,
    /// The track's title when the file has one.
    pub name: String,
    /// The container marks it as the one to play (Matroska `FlagDefault`, MP4 enabled track).
    pub default: bool,
}

/// What a container says about a track besides how to decode it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackMeta {
    pub language: String,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Video,
    Audio,
    Other,
}

/// The fourcc of an MXF track whose PCM is in AES3 element form (see [`Codec::Pcm`]).
pub const AES3: [u8; 4] = *b"aes3";

impl Codec {
    /// A short name for audio codecs, `"other"` for the rest.
    pub fn audio_name(&self) -> &'static str {
        match self {
            Codec::Aac { .. } => "AAC",
            Codec::Opus { .. } => "Opus",
            Codec::Vorbis { .. } => "Vorbis",
            Codec::Flac { .. } => "FLAC",
            Codec::Mp3 => "MP3",
            Codec::Ac3 { .. } => "AC-3",
            Codec::Eac3 { .. } => "E-AC-3",
            Codec::Pcm { .. } => "PCM",
            _ => "other",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Codec {
    /// H.264; the `avcC` box's payload (parameter sets, NAL length size).
    H264 { config: Vec<u8> },
    /// HEVC; the `hvcC` box's payload.
    Hevc { config: Vec<u8> },
    /// AAC; the AudioSpecificConfig from `esds`.
    Aac { config: Vec<u8> },
    /// VP9 (Matroska / WebM); the optional `CodecPrivate` (usually empty).
    Vp9 { config: Vec<u8> },
    /// AV1 (Matroska / WebM); the `av1C` payload.
    Av1 { config: Vec<u8> },
    /// Opus (Matroska / WebM); the `OpusHead` packet.
    Opus { config: Vec<u8> },
    /// Vorbis (Matroska / WebM); the three Xiph-laced header packets.
    Vorbis { config: Vec<u8> },
    /// FLAC; `fLaC` followed by the metadata blocks (Matroska `CodecPrivate`, or `dfLa`'s
    /// blocks with the marker put in front).
    Flac { config: Vec<u8> },
    /// MP3 (MPEG-1 / 2 Layer III).
    Mp3,
    /// AC-3; the `dac3` payload in MP4, empty in Matroska.
    Ac3 { config: Vec<u8> },
    /// E-AC-3; the `dec3` payload in MP4, empty in Matroska.
    Eac3 { config: Vec<u8> },
    /// Uncompressed audio. MXF's AES3 elements (4-byte header, 32-bit subframes) are a
    /// `Pcm { float: false, big_endian: false }` track whose fourcc is [`AES3`]: `Track::read_sample`
    /// returns them as plain interleaved PCM of `bits` per sample.
    Pcm { bits: u16, float: bool, big_endian: bool },
    /// Anything else, by its sample entry's four letters.
    Other,
}

#[derive(Debug, Clone)]
pub struct Track {
    pub id: u32,
    pub kind: Kind,
    /// The sample entry's four letters (`avc1`, `hvc1`, `mp4a`, `twos`, …).
    pub fourcc: [u8; 4],
    pub codec: Codec,
    /// Units per second of the sample times.
    pub timescale: u32,
    /// Coded size (video).
    pub width: u32,
    pub height: u32,
    /// Clockwise degrees (0, 90, 180, 270) to turn the picture for display.
    pub rotation: u16,
    pub channels: u16,
    pub sample_rate: u32,
    pub samples: Vec<Sample>,
    /// Colour description written in the container (Matroska `Colour`), if any.
    pub colour: Option<Colour>,
    /// The container marks this track as the one to play by default (always true for MP4).
    pub default: bool,
    /// Matroska `CodecDelay` in nanoseconds (Opus pre-skip), 0 otherwise.
    pub codec_delay_ns: u64,
    /// Matroska `DiscardPadding` of the last block, nanoseconds (Opus end trim), 0 otherwise.
    pub discard_padding_ns: u64,
    /// Pixel aspect ratio the container states (MP4 `pasp`, derived from Matroska
    /// `DisplayWidth` / `DisplayHeight`): width : height of one pixel. `None` when it says nothing.
    pub par: Option<(u32, u32)>,
    /// Where the sample bytes are and how they become what the decoders read.
    pub storage: Storage,
    /// Language and title.
    pub meta: TrackMeta,
}

/// How a track's samples are stored. [`Sample::offset`] and [`Sample::size`] always describe what
/// [`Track::read_sample`] returns once the storage is undone, except for the byte position:
/// [`Storage::Plain`] samples are one byte range of the file, the others are rebuilt on read.
#[derive(Debug, Clone, Default)]
pub enum Storage {
    /// One byte range at `offset`, `size` long, exactly as the decoder wants it (MP4, Matroska).
    #[default]
    Plain,
    /// MPEG transport stream: the bytes are spread over packets, see [`crate::ts::Layout`].
    Ts(std::sync::Arc<crate::ts::Layout>),
    /// One byte range holding Annex B NAL units (AVI): read as is, then turned into the length
    /// prefixed form.
    AnnexB(std::sync::Arc<AnnexB>),
}

/// AVI chunks of H.264 / HEVC in Annex B form.
#[derive(Debug, Clone)]
pub struct AnnexB {
    pub hevc: bool,
    /// Bytes of each chunk in the file (`Sample::size` is the length prefixed size).
    pub raw: Vec<u32>,
}

/// Matroska `Colour` element, values as ISO/IEC 23091-2 code points.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Colour {
    pub matrix: u8,
    pub transfer: u8,
    pub primaries: u8,
    /// `Some(true)` full range, `Some(false)` limited, `None` unspecified.
    pub full_range: Option<bool>,
    pub bits_per_channel: u8,
}

/// One sample: a compressed picture, or a block of audio.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    /// Byte position in the file.
    pub offset: u64,
    pub size: u32,
    /// Decode time, in the track's timescale.
    pub dts: i64,
    /// Presentation time, in the track's timescale.
    pub pts: i64,
    /// Decoding can start here (a sync sample).
    pub key: bool,
}

impl Track {
    pub fn seconds(&self, t: i64) -> f64 {
        if self.timescale == 0 { 0.0 } else { t as f64 / self.timescale as f64 }
    }

    /// Reads sample `i` into `buf` (replacing its contents).
    pub fn read_sample<R: Read + Seek>(&self, r: &mut R, i: usize, buf: &mut Vec<u8>) -> Result<()> {
        let s = self.samples.get(i).ok_or(Error::Invalid("sample index"))?;
        match &self.storage {
            Storage::Plain => {}
            Storage::Ts(l) => return l.read(r, s, i, buf),
            Storage::AnnexB(a) => {
                let mut raw = vec![0u8; *a.raw.get(i).ok_or(Error::Invalid("sample index"))? as usize];
                r.seek(SeekFrom::Start(s.offset))?;
                r.read_exact(&mut raw)?;
                buf.clear();
                crate::nal::annexb_to_length_prefixed(&raw, a.hevc, buf);
                return Ok(());
            }
        }
        buf.clear();
        buf.resize(s.size as usize, 0);
        r.seek(SeekFrom::Start(s.offset))?;
        r.read_exact(buf)?;
        match self.codec {
            // MXF keeps H.264 and HEVC as Annex B: the decoders read length-prefixed units
            Codec::H264 { .. } | Codec::Hevc { .. } if crate::annexb::is_annexb(buf) => {
                let raw = std::mem::take(buf);
                crate::annexb::to_length_prefixed(&raw, buf);
            }
            Codec::Pcm { bits, .. } if self.fourcc == AES3 => {
                let raw = std::mem::take(buf);
                crate::mxf::aes3_to_pcm(&raw, self.channels as usize, bits, buf);
            }
            _ => {}
        }
        Ok(())
    }

    /// Frames per second from the sample times (video), 0 when unknown.
    pub fn frame_rate(&self) -> f64 {
        let n = self.samples.len();
        if n < 2 || self.timescale == 0 {
            return 0.0;
        }
        let first = self.samples.iter().map(|s| s.dts).min().unwrap_or(0);
        let last = self.samples.iter().map(|s| s.dts).max().unwrap_or(0);
        if last <= first {
            return 0.0;
        }
        (n - 1) as f64 * self.timescale as f64 / (last - first) as f64
    }
}

/// Reads the structure of an ISO base media file.
pub fn read<R: Read + Seek>(r: &mut R) -> Result<Movie> {
    let end = r.seek(SeekFrom::End(0))?;
    let mut pos = 0u64;
    let mut moov = None;
    let mut moofs: Vec<(u64, Vec<u8>)> = Vec::new();
    while pos + 8 <= end {
        r.seek(SeekFrom::Start(pos))?;
        let mut h = [0u8; 16];
        r.read_exact(&mut h[..8])?;
        let size32 = u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as u64;
        let kind = [h[4], h[5], h[6], h[7]];
        let (size, header) = match size32 {
            0 => (end - pos, 8),
            1 => {
                r.read_exact(&mut h[8..16])?;
                (u64::from_be_bytes(h[8..16].try_into().unwrap()), 16)
            }
            n => (n, 8),
        };
        if size < header || pos.checked_add(size).is_none_or(|e| e > end) {
            // a cut file: what was read so far still counts (a cut `moof` is dropped, a cut
            // `mdat` loses the samples that were not fully written)
            if moov.is_some() {
                break;
            }
            return Err(Error::Invalid("top-level box runs past the end of the file"));
        }
        match &kind {
            b"moov" => {
                let len = size - header;
                if len > MOOV_MAX {
                    return Err(Error::Unsupported("movie header over 256 MB"));
                }
                let mut b = vec![0u8; len as usize];
                r.read_exact(&mut b)?;
                moov = Some(b);
            }
            b"moof" if moov.is_some() => {
                let len = size - header;
                if len > MOOV_MAX {
                    return Err(Error::Unsupported("fragment header over 256 MB"));
                }
                let mut b = vec![0u8; len as usize];
                r.read_exact(&mut b)?;
                moofs.push((pos, b));
            }
            _ => {}
        }
        pos += size;
    }
    let Some(moov) = moov else {
        return Err(Error::Invalid("no movie header (moov)"));
    };
    let (mut movie, mut extra) = parse_moov(&moov)?;
    if !moofs.is_empty() {
        for (pos, b) in &moofs {
            // one broken fragment ends the walk: what follows would be timed from a wrong place
            if let Err(e) = add_fragment(&mut movie.tracks, &mut extra, *pos, b) {
                log::debug!("mp4: fragment at {pos}: {e}");
                break;
            }
        }
        // a cut last `mdat`: only whole samples count
        for (t, e) in movie.tracks.iter_mut().zip(&extra) {
            let from = e.moov_samples.min(t.samples.len());
            let whole = t.samples[from..].iter().take_while(|s| s.offset + s.size as u64 <= end).count();
            t.samples.truncate(from + whole);
        }
        // `empty_moov` leaves the movie duration 0: the longest track's end
        if movie.duration == 0 && movie.timescale > 0 {
            movie.duration = movie
                .tracks
                .iter()
                .filter(|t| t.timescale > 0)
                .map(|t| {
                    let n = t.samples.len();
                    let last = t.samples.iter().map(|s| s.pts).max().unwrap_or(0).max(0);
                    let step = if n >= 2 { (t.samples[n - 1].dts - t.samples[n - 2].dts).max(0) } else { 0 };
                    ((last + step) as i128 * movie.timescale as i128 / t.timescale as i128) as u64
                })
                .max()
                .unwrap_or(0);
        }
    }
    Ok(movie)
}

/// What fragments need from `moov` per track, kept beside [`Track`].
#[derive(Debug, Clone, Default)]
struct Extra {
    /// Samples that came from `moov` itself (they stay when the file is cut).
    moov_samples: usize,
    /// Shift of the first edit, applied to fragment samples too.
    shift: i64,
    /// `trex` defaults.
    trex: Defaults,
    /// Decode time after the last sample so far (`tfdt` is missing in some files).
    next_dts: Option<i64>,
}

#[derive(Debug, Clone, Copy, Default)]
struct Defaults {
    duration: u32,
    size: u32,
    flags: u32,
}

/// Sample flags: not a sync sample, or depends on others (ISO 14496-12 8.8.3.1).
const NON_SYNC: u32 = 0x0001_0000;
const DEPENDS_YES: u32 = 0x0100_0000;

fn add_fragment(tracks: &mut [Track], extra: &mut [Extra], moof_pos: u64, moof: &[u8]) -> Result<()> {
    let boxes = children(moof)?;
    let mut prev_end: Option<u64> = None;
    for (k, traf) in &boxes {
        if k != b"traf" {
            continue;
        }
        let kids = children(traf)?;
        let tfhd = child(&kids, b"tfhd").ok_or(Error::Invalid("no track fragment header"))?;
        let mut c = Cur::new(tfhd);
        let (_, f) = c.full()?;
        let id = c.u32()?;
        let base_named = if f & 0x01 != 0 { Some(c.u64()?) } else { None };
        if f & 0x02 != 0 {
            c.skip(4)?;
        }
        let Some(ti) = tracks.iter().position(|t| t.id == id) else { continue };
        let mut d = extra[ti].trex;
        if f & 0x08 != 0 {
            d.duration = c.u32()?;
        }
        if f & 0x10 != 0 {
            d.size = c.u32()?;
        }
        if f & 0x20 != 0 {
            d.flags = c.u32()?;
        }
        // base of the data offsets: named, or the `moof` (default-base-is-moof, and the first
        // `traf` otherwise), or where the previous `traf`'s data ended
        let base = base_named.unwrap_or(if f & 0x02_0000 != 0 { moof_pos } else { prev_end.unwrap_or(moof_pos) });

        if let Some(tfdt) = child(&kids, b"tfdt") {
            let mut c = Cur::new(tfdt);
            let (v, _) = c.full()?;
            let t = if v == 1 { c.u64()? as i64 } else { c.u32()? as i64 };
            extra[ti].next_dts = Some(t + extra[ti].shift);
        }
        let mut data_end = base;
        let mut first_run = true;
        for (k, trun) in &kids {
            if k != b"trun" {
                continue;
            }
            let mut c = Cur::new(trun);
            let (v, f) = c.full()?;
            let n = c.u32()? as usize;
            let mut off = if f & 0x01 != 0 { (base as i64 + c.i32()? as i64) as u64 } else if first_run { base } else { data_end };
            let first_flags = if f & 0x04 != 0 { Some(c.u32()?) } else { None };
            first_run = false;
            let per = 4 * ((f & 0x100 != 0) as usize + (f & 0x200 != 0) as usize + (f & 0x400 != 0) as usize + (f & 0x800 != 0) as usize);
            let n = if per == 0 { n.min(SAMPLES_MAX) } else { n.min(c.left() / per) };
            let t = &mut tracks[ti];
            if t.samples.len() + n > SAMPLES_MAX {
                return Err(Error::Unsupported("too many samples"));
            }
            t.samples.reserve(n);
            for i in 0..n {
                let dur = if f & 0x100 != 0 { c.u32()? } else { d.duration };
                let size = if f & 0x200 != 0 { c.u32()? } else { d.size };
                let flags = if f & 0x400 != 0 { c.u32()? } else if i == 0 { first_flags.unwrap_or(d.flags) } else { d.flags };
                let cts = if f & 0x800 != 0 {
                    let raw = c.u32()?;
                    if v == 0 && raw <= i32::MAX as u32 { raw as i64 } else { raw as i32 as i64 }
                } else {
                    0
                };
                // no `tfdt`: continue where the track ended
                let dts = match extra[ti].next_dts {
                    Some(x) => x,
                    None => match t.samples.as_slice() {
                        [.., a, b] => b.dts + (b.dts - a.dts),
                        [b] => b.dts,
                        [] => extra[ti].shift,
                    },
                };
                t.samples.push(Sample { offset: off, size, dts, pts: dts + cts, key: flags & (NON_SYNC | DEPENDS_YES) == 0 });
                extra[ti].next_dts = Some(dts + dur as i64);
                off += size as u64;
            }
            data_end = data_end.max(off);
        }
        prev_end = Some(data_end);
    }
    Ok(())
}

// ------------------------------------------------------------------ byte cursor

struct Cur<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Cur<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, p: 0 }
    }
    fn left(&self) -> usize {
        self.b.len() - self.p
    }
    fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.left() < n {
            return Err(Error::Invalid("box cut short"));
        }
        let s = &self.b[self.p..self.p + n];
        self.p += n;
        Ok(s)
    }
    fn skip(&mut self, n: usize) -> Result<()> {
        self.bytes(n).map(|_| ())
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.bytes(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.bytes(4)?.try_into().unwrap()))
    }
    fn i32(&mut self) -> Result<i32> {
        Ok(self.u32()? as i32)
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.bytes(8)?.try_into().unwrap()))
    }
    fn fourcc(&mut self) -> Result<[u8; 4]> {
        Ok(self.bytes(4)?.try_into().unwrap())
    }
    /// Version and flags of a full box.
    fn full(&mut self) -> Result<(u8, u32)> {
        let v = self.u32()?;
        Ok(((v >> 24) as u8, v & 0x00ff_ffff))
    }
    fn rest(&mut self) -> &'a [u8] {
        let s = &self.b[self.p..];
        self.p = self.b.len();
        s
    }
}

/// The child boxes of a box's payload, as (type, payload).
fn children(b: &[u8]) -> Result<Vec<([u8; 4], &[u8])>> {
    let mut out = Vec::new();
    let mut c = Cur::new(b);
    while c.left() >= 8 {
        let size32 = c.u32()? as u64;
        let kind = c.fourcc()?;
        let (size, header) = match size32 {
            0 => (c.left() as u64 + 8, 8u64),
            1 => (c.u64()?, 16),
            n => (n, 8),
        };
        if size < header || size - header > c.left() as u64 {
            return Err(Error::Invalid("box runs past its parent"));
        }
        out.push((kind, c.bytes((size - header) as usize)?));
    }
    Ok(out)
}

fn child<'a>(boxes: &[([u8; 4], &'a [u8])], kind: &[u8; 4]) -> Option<&'a [u8]> {
    boxes.iter().find(|(k, _)| k == kind).map(|(_, b)| *b)
}

// ------------------------------------------------------------------ moov

fn parse_moov(b: &[u8]) -> Result<(Movie, Vec<Extra>)> {
    let boxes = children(b)?;
    let mvhd = child(&boxes, b"mvhd").ok_or(Error::Invalid("no movie header (mvhd)"))?;
    let mut c = Cur::new(mvhd);
    let (v, _) = c.full()?;
    let (timescale, duration) = if v == 1 {
        c.skip(16)?;
        (c.u32()?, c.u64()?)
    } else {
        c.skip(8)?;
        (c.u32()?, c.u32()? as u64)
    };
    // `mvex`: per-track defaults for the fragments
    let fragmented = child(&boxes, b"mvex").is_some();
    let mut trex: Vec<(u32, Defaults)> = Vec::new();
    if let Some(mvex) = child(&boxes, b"mvex") {
        for (k, t) in children(mvex).unwrap_or_default() {
            if &k == b"trex" {
                let mut c = Cur::new(t);
                let r = (|| -> Result<(u32, Defaults)> {
                    c.full()?;
                    let id = c.u32()?;
                    c.skip(4)?;
                    Ok((id, Defaults { duration: c.u32()?, size: c.u32()?, flags: c.u32()? }))
                })();
                if let Ok(x) = r {
                    trex.push(x);
                }
            }
        }
    }
    let mut tracks = Vec::new();
    let mut extra = Vec::new();
    for (k, t) in &boxes {
        if k == b"trak" {
            match parse_trak(t, timescale, fragmented) {
                Ok((t, shift)) => {
                    extra.push(Extra {
                        moov_samples: t.samples.len(),
                        shift,
                        trex: trex.iter().find(|x| x.0 == t.id).map_or(Defaults::default(), |x| x.1),
                        next_dts: None,
                    });
                    tracks.push(t);
                }
                // one broken track doesn't take the others down
                Err(e) => log::debug!("mp4: skipping a track: {e}"),
            }
        }
    }
    Ok((Movie { timescale, duration, tracks }, extra))
}

/// Clockwise degrees from a track header's matrix (screen coordinates, y down).
fn rotation(m: &[i32; 9]) -> u16 {
    let (a, b) = (m[0] as f64, m[1] as f64);
    let deg = b.atan2(a).to_degrees();
    (((deg / 90.0).round() as i32).rem_euclid(4) * 90) as u16
}

/// QuickTime language codes 0..=85 as ISO 639-2 (empty where the code has no plain equivalent).
const MAC_LANGUAGES: [&str; 86] = [
    "eng", "fra", "deu", "ita", "nld", "swe", "spa", "dan", "por", "nor", "heb", "jpn", "ara", "fin", "ell", "isl", "mlt", "tur", "hrv", "zho", "urd", "hin", "tha", "kor", "lit", "pol", "hun", "est",
    "lav", "", "fao", "fas", "rus", "zho", "nld", "gle", "sqi", "ron", "ces", "slk", "slv", "yid", "srp", "mkd", "bul", "ukr", "bel", "uzb", "kaz", "aze", "aze", "hye", "kat", "ron", "kir", "tgk",
    "tuk", "mon", "mon", "pus", "kur", "kas", "snd", "bod", "nep", "san", "mar", "ben", "asm", "guj", "pan", "ori", "mal", "kan", "tam", "tel", "sin", "mya", "khm", "lao", "vie", "ind", "tgl", "msa",
    "msa", "amh",
];

fn parse_trak(b: &[u8], movie_timescale: u32, fragmented: bool) -> Result<(Track, i64)> {
    let boxes = children(b)?;
    let tkhd = child(&boxes, b"tkhd").ok_or(Error::Invalid("no track header"))?;
    let mut c = Cur::new(tkhd);
    let (v, tkhd_flags) = c.full()?;
    let id = if v == 1 {
        c.skip(16)?;
        let id = c.u32()?;
        c.skip(4 + 8)?;
        id
    } else {
        c.skip(8)?;
        let id = c.u32()?;
        c.skip(4 + 4)?;
        id
    };
    c.skip(8 + 2 + 2 + 2 + 2)?;
    let mut m = [0i32; 9];
    for x in &mut m {
        *x = c.i32()?;
    }
    let rot = rotation(&m);

    let mdia = children(child(&boxes, b"mdia").ok_or(Error::Invalid("no media box"))?)?;
    let mdhd = child(&mdia, b"mdhd").ok_or(Error::Invalid("no media header"))?;
    let mut c = Cur::new(mdhd);
    let (v, _) = c.full()?;
    let timescale = if v == 1 {
        c.skip(16)?;
        c.u32()?
    } else {
        c.skip(8)?;
        c.u32()?
    };
    // duration, then the language as three letters of 5 bits each (letter - 0x60)
    let language = (|| -> Result<String> {
        c.skip(if v == 1 { 8 } else { 4 })?;
        let l = c.u16()?;
        if l < 0x400 {
            // QuickTime's older numbering (MOV files written by Apple software and ffmpeg)
            return Ok(MAC_LANGUAGES.get(l as usize).copied().unwrap_or("").to_string());
        }
        let s: String = (0..3).map(|i| (((l >> (10 - 5 * i)) & 31) as u8 + 0x60) as char).collect();
        Ok(if s.chars().all(|c| c.is_ascii_lowercase()) { s } else { String::new() })
    })()
    .unwrap_or_default();
    let handler_name = child(&mdia, b"hdlr").map(|h| {
        // version / flags, predefined, type, three reserved words, then the name (a C string,
        // or a length-prefixed one in QuickTime files)
        let b = h.get(24..).unwrap_or(&[]);
        let b = if !b.is_empty() && b[0] as usize == b.len() - 1 { &b[1..] } else { b };
        String::from_utf8_lossy(b).trim_end_matches('\0').trim().to_string()
    });
    let handler = child(&mdia, b"hdlr").map(|h| {
        let mut c = Cur::new(h);
        let _ = c.full();
        let _ = c.u32();
        c.fourcc().unwrap_or(*b"    ")
    });
    let kind = match handler.as_ref() {
        Some(b"vide") => Kind::Video,
        Some(b"soun") => Kind::Audio,
        _ => Kind::Other,
    };
    let minf = children(child(&mdia, b"minf").ok_or(Error::Invalid("no media information"))?)?;
    let stbl = children(child(&minf, b"stbl").ok_or(Error::Invalid("no sample table"))?)?;

    let mut t = Track {
        id,
        kind,
        fourcc: *b"    ",
        codec: Codec::Other,
        timescale,
        width: 0,
        height: 0,
        rotation: if kind == Kind::Video { rot } else { 0 },
        channels: 0,
        sample_rate: 0,
        samples: Vec::new(),
        colour: None,
        default: true,
        codec_delay_ns: 0,
        discard_padding_ns: 0,
        par: None,
        storage: Storage::Plain,
        meta: TrackMeta::default(),
    };
    t.default = tkhd_flags & 1 != 0;
    t.meta.language = if language == "und" { String::new() } else { language };
    // a title: `udta/name`, else the handler's name unless it is the muxer's stock text
    let udta_name = child(&boxes, b"udta").and_then(|u| children(u).ok()).and_then(|k| child(&k, b"name").map(|n| String::from_utf8_lossy(n).trim_end_matches('\0').trim().to_string()));
    let stock = |n: &str| n.is_empty() || n.ends_with("Handler") || n.contains("Media Handler") || n == "Core Media Audio" || n == "Core Media Video";
    t.meta.name = match (udta_name, handler_name) {
        (Some(n), _) if !n.is_empty() => n,
        (_, Some(n)) if !stock(&n) => n,
        _ => String::new(),
    };
    if let Some(stsd) = child(&stbl, b"stsd") {
        parse_stsd(stsd, &mut t)?;
    }
    let table = if kind == Kind::Audio && matches!(t.codec, Codec::Pcm { .. }) { pcm_chunks(&stbl) } else { samples(&stbl) };
    t.samples = match table {
        Ok(s) => s,
        // a fragmented file may leave the tables out altogether
        Err(_) if fragmented => Vec::new(),
        Err(e) => return Err(e),
    };
    let shift = edit_shift(child(&boxes, b"edts"), movie_timescale, timescale)?;
    if shift != 0 {
        for s in &mut t.samples {
            s.dts += shift;
            s.pts += shift;
        }
    }
    Ok((t, shift))
}

/// How far the first edit moves the track's times: an empty edit delays it, a media time
/// skips into it (encoder delay, B-frame reordering).
fn edit_shift(edts: Option<&[u8]>, movie_timescale: u32, timescale: u32) -> Result<i64> {
    let Some(edts) = edts else { return Ok(0) };
    let Some(elst) = child(&children(edts)?, b"elst") else { return Ok(0) };
    let mut c = Cur::new(elst);
    let (v, _) = c.full()?;
    let n = c.u32()?;
    let mut delay = 0i64;
    for _ in 0..n.min(64) {
        let (dur, media_time) = if v == 1 { (c.u64()? as i64, c.u64()? as i64) } else { (c.u32()? as i64, c.i32()? as i64) };
        c.skip(4)?;
        if media_time == -1 {
            // empty edit: nothing shown for `dur` (movie timescale)
            if movie_timescale > 0 {
                delay += (dur as i128 * timescale as i128 / movie_timescale as i128) as i64;
            }
            continue;
        }
        return Ok(delay - media_time);
    }
    Ok(delay)
}

// ------------------------------------------------------------------ sample description

fn parse_stsd(b: &[u8], t: &mut Track) -> Result<()> {
    let mut c = Cur::new(b);
    c.full()?;
    let n = c.u32()?;
    let entries = children(c.rest())?;
    let Some((fourcc, e)) = entries.into_iter().take(n as usize).next() else { return Ok(()) };
    t.fourcc = fourcc;
    match t.kind {
        Kind::Video => {
            let mut c = Cur::new(e);
            c.skip(6 + 2 + 2 + 2 + 12)?;
            t.width = c.u16()? as u32;
            t.height = c.u16()? as u32;
            c.skip(4 + 4 + 4 + 2 + 32 + 2 + 2)?;
            let kids = children(c.rest())?;
            if let Some(p) = child(&kids, b"pasp").filter(|p| p.len() >= 8) {
                let (h, v) = (u32::from_be_bytes([p[0], p[1], p[2], p[3]]), u32::from_be_bytes([p[4], p[5], p[6], p[7]]));
                if h > 0 && v > 0 {
                    t.par = Some((h, v));
                }
            }
            t.codec = match &fourcc {
                b"avc1" | b"avc3" => child(&kids, b"avcC").map_or(Codec::Other, |x| Codec::H264 { config: x.to_vec() }),
                b"hvc1" | b"hev1" => child(&kids, b"hvcC").map_or(Codec::Other, |x| Codec::Hevc { config: x.to_vec() }),
                b"av01" => child(&kids, b"av1C").map_or(Codec::Other, |x| Codec::Av1 { config: x.to_vec() }),
                _ => Codec::Other,
            };
        }
        Kind::Audio => {
            let mut c = Cur::new(e);
            c.skip(6 + 2)?;
            let version = c.u16()?;
            c.skip(2 + 4)?;
            let mut channels = c.u16()?;
            let mut bits = c.u16()?;
            c.skip(2 + 2)?;
            let mut rate = c.u32()? >> 16;
            let mut lpcm_flags = 0u32;
            match version {
                1 => c.skip(16)?,
                2 => {
                    c.skip(4)?;
                    rate = f64::from_bits(c.u64()?) as u32;
                    channels = c.u32()? as u16;
                    c.skip(4)?;
                    bits = c.u32()? as u16;
                    lpcm_flags = c.u32()?;
                    c.skip(8)?;
                }
                _ => {}
            }
            t.channels = channels;
            t.sample_rate = rate;
            let kids = children(c.rest()).unwrap_or_default();
            t.codec = match &fourcc {
                b"mp4a" => match esds(&kids) {
                    // object type 0x6B: MPEG-1 audio, 0x69: MPEG-2 audio (Layer III in practice)
                    Some((0x6B | 0x69, _)) => Codec::Mp3,
                    Some((_, Some(config))) => Codec::Aac { config },
                    _ => Codec::Other,
                },
                b".mp3" => Codec::Mp3,
                b"Opus" => child(&kids, b"dOps").and_then(|d| opus_head(d, channels, rate)).map_or(Codec::Other, |config| Codec::Opus { config }),
                b"fLaC" => child(&kids, b"dfLa").filter(|d| d.len() > 4).map_or(Codec::Other, |d| {
                    let mut config = b"fLaC".to_vec();
                    config.extend_from_slice(&d[4..]);
                    Codec::Flac { config }
                }),
                b"ac-3" => Codec::Ac3 { config: child(&kids, b"dac3").unwrap_or_default().to_vec() },
                b"ec-3" => Codec::Eac3 { config: child(&kids, b"dec3").unwrap_or_default().to_vec() },
                b"twos" => Codec::Pcm { bits: if bits == 8 { 8 } else { 16 }, float: false, big_endian: true },
                b"sowt" => Codec::Pcm { bits: 16, float: false, big_endian: false },
                b"in24" => Codec::Pcm { bits: 24, float: false, big_endian: !little_endian(&kids) },
                b"in32" => Codec::Pcm { bits: 32, float: false, big_endian: !little_endian(&kids) },
                b"fl32" => Codec::Pcm { bits: 32, float: true, big_endian: !little_endian(&kids) },
                b"fl64" => Codec::Pcm { bits: 64, float: true, big_endian: !little_endian(&kids) },
                // QuickTime LPCM: flags 1 = float, 2 = big endian
                b"lpcm" => Codec::Pcm { bits, float: lpcm_flags & 1 != 0, big_endian: lpcm_flags & 2 != 0 },
                b"ipcm" | b"fpcm" => {
                    let le = child(&kids, b"pcmC").and_then(|p| p.get(4)).is_some_and(|f| f & 1 != 0);
                    Codec::Pcm { bits, float: &fourcc == b"fpcm", big_endian: !le }
                }
                _ => Codec::Other,
            };
        }
        Kind::Other => {}
    }
    Ok(())
}

/// QuickTime keeps byte order in `wave` → `enda`.
fn little_endian(kids: &[([u8; 4], &[u8])]) -> bool {
    child(kids, b"wave").and_then(|w| children(w).ok()).and_then(|w| child(&w, b"enda").map(|e| e.get(1) == Some(&1) || e.first() == Some(&1))).unwrap_or(false)
}

/// An `OpusHead` packet built from `dOps` (the same fields, big endian there, little endian
/// in the packet).
fn opus_head(d: &[u8], channels: u16, rate: u32) -> Option<Vec<u8>> {
    // version, channel count, pre-skip, input rate, output gain, mapping family [, mapping]
    if d.len() < 11 {
        return None;
    }
    let _ = (channels, rate);
    let mut h = b"OpusHead".to_vec();
    h.push(1);
    h.push(d[1]);
    h.extend_from_slice(&u16::from_be_bytes([d[2], d[3]]).to_le_bytes());
    h.extend_from_slice(&u32::from_be_bytes([d[4], d[5], d[6], d[7]]).to_le_bytes());
    h.extend_from_slice(&i16::from_be_bytes([d[8], d[9]]).to_le_bytes());
    h.extend_from_slice(&d[10..]);
    Some(h)
}

/// What `esds` says: the object type indication and the AudioSpecificConfig, if any (directly,
/// or inside QuickTime's `wave`).
fn esds(kids: &[([u8; 4], &[u8])]) -> Option<(u8, Option<Vec<u8>>)> {
    let esds = child(kids, b"esds").or_else(|| {
        let wave = child(kids, b"wave")?;
        let w = children(wave).ok()?;
        child(&w, b"esds")
    })?;
    let mut c = Cur::new(esds);
    c.full().ok()?;
    // descriptors: tag, length in 7-bit groups
    fn desc<'a>(c: &mut Cur<'a>) -> Option<(u8, Cur<'a>)> {
        let tag = c.u8().ok()?;
        let mut len = 0usize;
        for _ in 0..4 {
            let b = c.u8().ok()?;
            len = (len << 7) | (b & 0x7f) as usize;
            if b & 0x80 == 0 {
                break;
            }
        }
        Some((tag, Cur::new(c.bytes(len.min(c.left())).ok()?)))
    }
    let (tag, mut es) = desc(&mut c)?;
    if tag != 3 {
        return None;
    }
    es.skip(2).ok()?;
    let flags = es.u8().ok()?;
    if flags & 0x80 != 0 {
        es.skip(2).ok()?;
    }
    if flags & 0x40 != 0 {
        let n = es.u8().ok()? as usize;
        es.skip(n).ok()?;
    }
    if flags & 0x20 != 0 {
        es.skip(2).ok()?;
    }
    let (tag, mut dc) = desc(&mut es)?;
    if tag != 4 {
        return None;
    }
    let oti = dc.u8().ok()?;
    dc.skip(12).ok()?;
    let asc = desc(&mut dc).and_then(|(tag, info)| (tag == 5).then(|| info.b.to_vec()));
    Some((oti, asc))
}

// ------------------------------------------------------------------ sample tables

/// Uncompressed audio lists every sample (one per channel group: 48 000 a second), so its
/// samples are taken a chunk at a time instead: one entry per chunk, its size the chunk's bytes.
fn pcm_chunks(stbl: &[([u8; 4], &[u8])]) -> Result<Vec<Sample>> {
    let Some(stsz) = child(stbl, b"stsz") else { return samples(stbl) };
    let mut c = Cur::new(stsz);
    c.full()?;
    let size = c.u32()? as u64;
    let count = c.u32()? as u64;
    if size == 0 {
        return samples(stbl);
    }
    // decode times as runs (count, delta), walked alongside the chunks
    let mut stts = Vec::new();
    if let Some(b) = child(stbl, b"stts") {
        let mut c = Cur::new(b);
        c.full()?;
        let n = (c.u32()? as usize).min(c.left() / 8);
        for _ in 0..n {
            stts.push((c.u32()? as u64, c.u32()? as i64));
        }
    }
    let chunks: Vec<u64> = if let Some(stco) = child(stbl, b"stco") {
        let mut c = Cur::new(stco);
        c.full()?;
        let n = (c.u32()? as usize).min(c.left() / 4);
        (0..n).map(|_| c.u32().map(|x| x as u64)).collect::<Result<_>>()?
    } else if let Some(co64) = child(stbl, b"co64") {
        let mut c = Cur::new(co64);
        c.full()?;
        let n = (c.u32()? as usize).min(c.left() / 8);
        (0..n).map(|_| c.u64()).collect::<Result<_>>()?
    } else {
        return Err(Error::Invalid("no chunk offsets"));
    };
    let stsc = child(stbl, b"stsc").ok_or(Error::Invalid("no sample-to-chunk table"))?;
    let mut c = Cur::new(stsc);
    c.full()?;
    let n = (c.u32()? as usize).min(c.left() / 12);
    let mut runs = Vec::with_capacity(n);
    for _ in 0..n {
        let first = c.u32()?;
        let per = c.u32()?;
        c.skip(4)?;
        runs.push((first.max(1) as usize, per as u64));
    }
    let (mut run, mut left, mut t, mut si) = (0usize, count, 0i64, 0usize);
    let mut in_run = stts.first().map_or(0, |r| r.0);
    let mut out = Vec::with_capacity(chunks.len());
    for (ci, &offset) in chunks.iter().enumerate() {
        while run + 1 < runs.len() && runs[run + 1].0 <= ci + 1 {
            run += 1;
        }
        let per = runs.get(run).filter(|r| r.0 <= ci + 1).map_or(0, |r| r.1).min(left);
        if per == 0 {
            if left == 0 {
                break;
            }
            continue;
        }
        let dts = t;
        // advance the clock by `per` samples
        let mut k = per;
        while k > 0 && si < stts.len() {
            let take = k.min(in_run);
            t += take as i64 * stts[si].1;
            k -= take;
            in_run -= take;
            if in_run == 0 {
                si += 1;
                in_run = stts.get(si).map_or(0, |r| r.0);
            }
        }
        let bytes = (per * size).min(u32::MAX as u64) as u32;
        out.push(Sample { offset, size: bytes, dts, pts: dts, key: true });
        left -= per;
    }
    Ok(out)
}

fn samples(stbl: &[([u8; 4], &[u8])]) -> Result<Vec<Sample>> {
    // sizes
    let mut sizes: Vec<u32> = Vec::new();
    let mut count;
    let mut constant = None;
    if let Some(stsz) = child(stbl, b"stsz") {
        let mut c = Cur::new(stsz);
        c.full()?;
        let size = c.u32()?;
        count = c.u32()? as usize;
        if size != 0 {
            constant = Some(size);
        } else {
            count = count.min(c.left() / 4);
            sizes.reserve(count);
            for _ in 0..count {
                sizes.push(c.u32()?);
            }
        }
    } else if let Some(stz2) = child(stbl, b"stz2") {
        let mut c = Cur::new(stz2);
        c.full()?;
        c.skip(3)?;
        let field = c.u8()?;
        count = c.u32()? as usize;
        match field {
            4 => {
                count = count.min(c.left() * 2);
                for i in 0..count {
                    let b = c.b[c.p + i / 2];
                    sizes.push(if i % 2 == 0 { (b >> 4) as u32 } else { (b & 15) as u32 });
                }
            }
            8 => {
                count = count.min(c.left());
                for _ in 0..count {
                    sizes.push(c.u8()? as u32);
                }
            }
            16 => {
                count = count.min(c.left() / 2);
                for _ in 0..count {
                    sizes.push(c.u16()? as u32);
                }
            }
            _ => return Err(Error::Invalid("compact sample size field")),
        }
    } else {
        return Ok(Vec::new());
    }

    // decode times
    let mut dts: Vec<i64> = Vec::new();
    if let Some(stts) = child(stbl, b"stts") {
        let mut c = Cur::new(stts);
        c.full()?;
        let n = (c.u32()? as usize).min(c.left() / 8);
        let mut t = 0i64;
        'outer: for _ in 0..n {
            let k = c.u32()?;
            let delta = c.u32()? as i64;
            for _ in 0..k {
                if dts.len() >= count.min(SAMPLES_MAX) {
                    break 'outer;
                }
                dts.push(t);
                t += delta;
            }
        }
    }
    count = count.min(dts.len()).min(SAMPLES_MAX);
    if constant.is_some() {
        sizes = vec![constant.unwrap(); count];
    }
    count = count.min(sizes.len());

    // composition offsets
    let mut cts = vec![0i64; count];
    if let Some(ctts) = child(stbl, b"ctts") {
        let mut c = Cur::new(ctts);
        let (v, _) = c.full()?;
        let n = (c.u32()? as usize).min(c.left() / 8);
        let mut i = 0usize;
        for _ in 0..n {
            let k = c.u32()? as usize;
            let raw = c.u32()?;
            let off = if v == 0 && raw <= i32::MAX as u32 { raw as i64 } else { raw as i32 as i64 };
            for _ in 0..k {
                if i >= count {
                    break;
                }
                cts[i] = off;
                i += 1;
            }
        }
    }

    // chunk offsets
    let chunks: Vec<u64> = if let Some(stco) = child(stbl, b"stco") {
        let mut c = Cur::new(stco);
        c.full()?;
        let n = (c.u32()? as usize).min(c.left() / 4);
        (0..n).map(|_| c.u32().map(|x| x as u64)).collect::<Result<_>>()?
    } else if let Some(co64) = child(stbl, b"co64") {
        let mut c = Cur::new(co64);
        c.full()?;
        let n = (c.u32()? as usize).min(c.left() / 8);
        (0..n).map(|_| c.u64()).collect::<Result<_>>()?
    } else {
        return Err(Error::Invalid("no chunk offsets"));
    };

    // samples per chunk: runs of (first chunk, samples per chunk)
    let stsc = child(stbl, b"stsc").ok_or(Error::Invalid("no sample-to-chunk table"))?;
    let mut c = Cur::new(stsc);
    c.full()?;
    let n = (c.u32()? as usize).min(c.left() / 12);
    let mut runs = Vec::with_capacity(n);
    for _ in 0..n {
        let first = c.u32()?;
        let per = c.u32()?;
        c.skip(4)?;
        runs.push((first.max(1) as usize, per as usize));
    }

    // sync samples (none listed: every sample is one)
    let mut key = vec![child(stbl, b"stss").is_none(); count];
    if let Some(stss) = child(stbl, b"stss") {
        let mut c = Cur::new(stss);
        c.full()?;
        let n = (c.u32()? as usize).min(c.left() / 4);
        for _ in 0..n {
            let i = c.u32()? as usize;
            if (1..=count).contains(&i) {
                key[i - 1] = true;
            }
        }
    }

    let mut out = Vec::with_capacity(count);
    let mut i = 0usize;
    let mut run = 0usize;
    'chunks: for (ci, &base) in chunks.iter().enumerate() {
        let chunk_no = ci + 1;
        while run + 1 < runs.len() && runs[run + 1].0 <= chunk_no {
            run += 1;
        }
        let per = runs.get(run).filter(|r| r.0 <= chunk_no).map_or(0, |r| r.1);
        let mut off = base;
        for _ in 0..per {
            if i >= count {
                break 'chunks;
            }
            out.push(Sample { offset: off, size: sizes[i], dts: dts[i], pts: dts[i] + cts[i], key: key[i] });
            off += sizes[i] as u64;
            i += 1;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn rotation_from_the_matrix() {
        let one = 0x10000;
        assert_eq!(super::rotation(&[one, 0, 0, 0, one, 0, 0, 0, 0x4000_0000]), 0);
        assert_eq!(super::rotation(&[0, one, 0, -one, 0, 0, 0, 0, 0x4000_0000]), 90);
        assert_eq!(super::rotation(&[-one, 0, 0, 0, -one, 0, 0, 0, 0x4000_0000]), 180);
        assert_eq!(super::rotation(&[0, -one, 0, one, 0, 0, 0, 0, 0x4000_0000]), 270);
    }
}
