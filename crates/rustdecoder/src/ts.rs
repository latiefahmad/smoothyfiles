//! MPEG transport streams: `.ts`, and `.m2ts` / `.mts` (AVCHD, Blu-ray) whose 188-byte packets
//! carry a 4-byte arrival time in front.
//!
//! [`read`] scans the whole file once, in large blocks, looking only at packet headers, the
//! PAT / PMT and the PES layer, and builds the same flat sample table the other containers
//! give. A sample is not one byte range here: a picture's PES payload is spread over every
//! 184th byte-run of the file, interleaved with other streams. So a sample is stored as
//! "the packet where it starts, how many elementary stream bytes to skip, how many to take"
//! ([`Layout`]); [`Layout::read`] walks the packets from there, keeps the bytes of the stream's
//! PID, cuts the PES headers and, for H.264 / HEVC, turns Annex B into the length prefixed form
//! the decoders read from MP4 (access unit delimiters and filler dropped).
//!
//! What is a sample:
//! - H.264 / HEVC: one PES = one access unit (broadcast and camera streams are made so). Parameter
//!   sets travel in-band; the first SPS / PPS (/ VPS) make the `avcC` / `hvcC` the track gets.
//!   Keyframes are IDR (H.264, also a recovery point SEI) or IRAP (HEVC) pictures.
//! - AAC (ADTS): one frame, header stripped; the AudioSpecificConfig is made from the first header.
//! - AC-3 / E-AC-3 / MPEG audio: one syncframe.
//! - LPCM of Blu-ray (stream type 0x80): one PES, the 4-byte header cut.
//! - MPEG-2 video: one PES per picture, listed (sync = I picture), not decoded.
//! - Anything else (AAC in LATM, DTS, TrueHD, subtitles, teletext, …) is listed without samples.
//!
//! Times are 90 kHz for video (audio: its sample rate) and are shifted so the earliest
//! presentation time of the file is 0. The 33-bit counters are unwrapped per stream.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

use crate::bits::BitReader;
use crate::mp4::{Codec, Kind, Movie, Sample, Storage, Track};
use crate::{Error, Result, h264, hevc, nal};

const SYNC: u8 = 0x47;
/// Packets read per block while indexing.
const BLOCK_PACKETS: usize = 16384;
/// Samples per stream at most.
const SAMPLES_MAX: usize = 50_000_000;
const WRAP: i64 = 1 << 33;

/// Packet size (188, or 192 with the arrival time) if `head` (the first bytes of a file) looks
/// like a transport stream: a sync byte at the start of each of the first few packets.
pub fn detect(head: &[u8]) -> Option<usize> {
    for packet in [188usize, 192] {
        let first = packet - 188; // 4 for M2TS
        let n = ((head.len().saturating_sub(first)) / packet).min(4);
        if n >= 2 && (0..n).all(|k| head[first + k * packet] == SYNC) {
            return Some(packet);
        }
    }
    None
}

/// Where the samples of one transport stream track are, see the module description.
#[derive(Debug)]
pub struct Layout {
    /// 188, or 192 for M2TS; 0 for an MPEG program stream (see [`crate::ps`]), whose samples
    /// are found by walking its PES packets.
    pub packet: usize,
    /// The PID; for a program stream the stream id, with the private stream 1 substream id
    /// (`0x80..`) in the high byte.
    pub pid: u16,
    /// `Some(is_hevc)` when the stream is Annex B and gets converted on read.
    pub annexb: Option<bool>,
    /// Elementary stream bytes to skip from the start of the sample's first packet (after its
    /// PES header, if it has one).
    pub(crate) skip: Vec<u32>,
    /// Elementary stream bytes of the sample as stored (before any conversion).
    pub(crate) raw: Vec<u32>,
}

impl Layout {
    /// Bytes of sample `i` in the stream, before Annex B conversion; for H.264 / HEVC this is
    /// what `ffprobe -show_packets` calls the packet size.
    pub fn raw_size(&self, i: usize) -> u32 {
        self.raw[i]
    }

    /// The sample's elementary stream bytes as they are in the stream (no conversion).
    pub fn read_raw<R: Read + Seek>(&self, r: &mut R, s: &Sample, i: usize, out: &mut Vec<u8>) -> Result<()> {
        out.clear();
        if self.packet == 0 {
            return self.read_raw_ps(r, s, i, out);
        }
        let want = *self.raw.get(i).ok_or(Error::Invalid("sample index"))? as usize;
        let mut skip = self.skip[i] as usize;
        let mut hdr_skip = 0usize;
        r.seek(SeekFrom::Start(s.offset))?;
        let mut block = vec![0u8; self.packet * 96];
        while out.len() < want {
            let n = read_full(r, &mut block)?;
            let whole = n / self.packet * self.packet;
            if whole == 0 {
                break;
            }
            for p in block[..whole].chunks_exact(self.packet) {
                let ts = &p[self.packet - 188..];
                if ts[0] != SYNC {
                    return Err(Error::Invalid("transport stream lost sync inside a sample"));
                }
                if pid_of(ts) != self.pid {
                    continue;
                }
                let Some(mut pay) = payload(ts) else { continue };
                if ts[1] & 0x40 != 0 {
                    let (h, _, _) = pes_header(pay).ok_or(Error::Invalid("PES header cut short"))?;
                    hdr_skip = h;
                }
                let k = hdr_skip.min(pay.len());
                pay = &pay[k..];
                hdr_skip -= k;
                let k = skip.min(pay.len());
                pay = &pay[k..];
                skip -= k;
                let take = pay.len().min(want - out.len());
                out.extend_from_slice(&pay[..take]);
                if out.len() == want {
                    return Ok(());
                }
            }
            if n < block.len() {
                break;
            }
        }
        Err(Error::Invalid("transport stream sample cut short"))
    }

    /// Program stream: walks the PES packets from the sample's first one, keeps those of the
    /// stream, cuts their headers (and the 4-byte private header of AC-3 in private stream 1).
    fn read_raw_ps<R: Read + Seek>(&self, r: &mut R, s: &Sample, i: usize, out: &mut Vec<u8>) -> Result<()> {
        let want = self.raw[i] as usize;
        let mut skip = self.skip[i] as usize;
        let (sid, sub) = ((self.pid & 0xff) as u8, (self.pid >> 8) as u8);
        let mut pos = s.offset;
        loop {
            r.seek(SeekFrom::Start(pos))?;
            let mut h = [0u8; 14];
            if read_full(r, &mut h[..6])? < 6 || h[..3] != [0, 0, 1] {
                return Err(Error::Invalid("program stream sample cut short"));
            }
            match h[3] {
                0xBA => {
                    read_full(r, &mut h[6..14])?;
                    pos += if h[4] >> 6 == 1 { 14 + (h[13] & 7) as u64 } else { 12 };
                    continue;
                }
                0xB9 => return Err(Error::Invalid("program stream sample cut short")),
                _ => {}
            }
            let len = u16::from_be_bytes([h[4], h[5]]) as usize;
            if h[3] == sid {
                let mut pkt = vec![0u8; 6 + len];
                pkt[..6].copy_from_slice(&h[..6]);
                r.read_exact(&mut pkt[6..])?;
                let (hl, _, _) = crate::ps::pes_header(&pkt).ok_or(Error::Invalid("PES header cut short"))?;
                let mut body = pkt.get(hl..).unwrap_or(&[]);
                if sub != 0 {
                    if body.first() != Some(&sub) {
                        pos += 6 + len as u64;
                        continue;
                    }
                    body = body.get(4..).unwrap_or(&[]);
                }
                let k = skip.min(body.len());
                body = &body[k..];
                skip -= k;
                let take = body.len().min(want - out.len());
                out.extend_from_slice(&body[..take]);
                if out.len() == want {
                    return Ok(());
                }
            }
            pos += 6 + len as u64;
        }
    }

    /// Reads sample `i` the way the decoders want it.
    pub fn read<R: Read + Seek>(&self, r: &mut R, s: &Sample, i: usize, buf: &mut Vec<u8>) -> Result<()> {
        match self.annexb {
            None => self.read_raw(r, s, i, buf),
            Some(hevc) => {
                let mut raw = Vec::new();
                self.read_raw(r, s, i, &mut raw)?;
                buf.clear();
                nal::annexb_to_length_prefixed(&raw, hevc, buf);
                Ok(())
            }
        }
    }
}

fn read_full<R: Read>(r: &mut R, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

pub(crate) fn pid_of(ts: &[u8]) -> u16 {
    (((ts[1] & 0x1f) as u16) << 8) | ts[2] as u16
}

/// The payload of a 188-byte packet, `None` when it has none.
fn payload(ts: &[u8]) -> Option<&[u8]> {
    let afc = (ts[3] >> 4) & 3;
    if afc & 1 == 0 {
        return None;
    }
    let mut o = 4usize;
    if afc & 2 != 0 {
        o += 1 + ts[4] as usize;
    }
    ts.get(o..).filter(|p| !p.is_empty())
}

fn ts_time(b: &[u8]) -> i64 {
    (((b[0] >> 1) & 7) as i64) << 30 | (b[1] as i64) << 22 | ((b[2] >> 1) as i64) << 15 | (b[3] as i64) << 7 | (b[4] >> 1) as i64
}

/// A PES header at the start of `pay`: its total length (up to the first payload byte), PTS and
/// DTS. `None` when the packet doesn't start with `00 00 01 sid`.
pub(crate) fn pes_header(pay: &[u8]) -> Option<(usize, Option<i64>, Option<i64>)> {
    if pay.len() < 6 || pay[..3] != [0, 0, 1] {
        return None;
    }
    // stream ids without the optional header (program stream map, padding, private 2, ECM, EMM, …)
    if matches!(pay[3], 0xBC | 0xBE | 0xBF | 0xF0 | 0xF1 | 0xF2 | 0xF8 | 0xFF) {
        return Some((6, None, None));
    }
    if pay.len() < 9 {
        return None;
    }
    let flags = pay[7] >> 6;
    let total = 9 + pay[8] as usize;
    let (mut pts, mut dts) = (None, None);
    if flags & 2 != 0 && pay.len() >= 14 {
        pts = Some(ts_time(&pay[9..14]));
        dts = if flags == 3 && pay.len() >= 19 { Some(ts_time(&pay[14..19])) } else { pts };
    }
    Some((total, pts, dts))
}

// ---------------------------------------------------------------------------------------------
// Program tables

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Format {
    H264,
    Hevc,
    Mpeg2Video,
    Adts,
    Latm,
    Mpa,
    Ac3,
    Eac3,
    Lpcm,
    /// Listed, not read: the stream type's name.
    Other(&'static str),
}

pub(crate) struct PmtEntry {
    pub(crate) pid: u16,
    pub(crate) stype: u8,
    pub(crate) format: Format,
}

fn classify(stype: u8, descriptors: &[u8]) -> Format {
    match stype {
        0x01 | 0x02 => Format::Mpeg2Video,
        0x1B => Format::H264,
        0x24 => Format::Hevc,
        0x0F => Format::Adts,
        0x11 => Format::Latm,
        0x03 | 0x04 => Format::Mpa,
        0x81 => Format::Ac3,
        0x84 | 0x87 | 0xA1 => Format::Eac3,
        0x80 => Format::Lpcm,
        0x82 | 0x85 | 0x86 | 0xA2 => Format::Other("DTS"),
        0x83 => Format::Other("TrueHD"),
        0xEA => Format::Other("VC-1"),
        0x90 => Format::Other("PGS subtitles"),
        0x06 => {
            let mut p = 0usize;
            while p + 2 <= descriptors.len() {
                let tag = descriptors[p];
                let len = descriptors[p + 1] as usize;
                let body = descriptors.get(p + 2..p + 2 + len).unwrap_or(&[]);
                match tag {
                    0x6A => return Format::Ac3,
                    0x7A => return Format::Eac3,
                    0x7B => return Format::Other("DTS"),
                    0x56 => return Format::Other("teletext"),
                    0x59 => return Format::Other("DVB subtitles"),
                    0x05 if body.starts_with(b"AC-3") => return Format::Ac3,
                    0x05 if body.starts_with(b"EAC3") => return Format::Eac3,
                    _ => {}
                }
                p += 2 + len;
            }
            Format::Other("private data")
        }
        _ => Format::Other("unknown stream type"),
    }
}

/// Collects the sections of one PID.
#[derive(Default)]
struct Section {
    buf: Vec<u8>,
}

impl Section {
    /// Feeds a packet's payload; returns a complete section.
    fn push(&mut self, pusi: bool, pay: &[u8]) -> Option<Vec<u8>> {
        if pusi {
            let ptr = *pay.first()? as usize;
            self.buf.clear();
            self.buf.extend_from_slice(pay.get(1 + ptr..)?);
        } else if !self.buf.is_empty() {
            self.buf.extend_from_slice(pay);
        } else {
            return None;
        }
        if self.buf.len() < 3 {
            return None;
        }
        let len = 3 + (((self.buf[1] & 0xf) as usize) << 8 | self.buf[2] as usize);
        if self.buf.len() < len {
            return None;
        }
        let s = self.buf[..len].to_vec();
        self.buf.clear();
        Some(s)
    }
}

fn parse_pat(s: &[u8]) -> Vec<u16> {
    let mut out = Vec::new();
    if s.len() < 12 || s[0] != 0 {
        return out;
    }
    let mut p = 8;
    while p + 4 <= s.len() - 4 {
        let program = u16::from_be_bytes([s[p], s[p + 1]]);
        let pid = u16::from_be_bytes([s[p + 2] & 0x1f, s[p + 3]]);
        if program != 0 {
            out.push(pid);
        }
        p += 4;
    }
    out
}

fn parse_pmt(s: &[u8]) -> Vec<PmtEntry> {
    let mut out = Vec::new();
    if s.len() < 16 || s[0] != 2 {
        return out;
    }
    let pil = (((s[10] & 0xf) as usize) << 8) | s[11] as usize;
    let mut p = 12 + pil;
    let end = s.len() - 4;
    while p + 5 <= end {
        let stype = s[p];
        let pid = u16::from_be_bytes([s[p + 1] & 0x1f, s[p + 2]]);
        let il = (((s[p + 3] & 0xf) as usize) << 8) | s[p + 4] as usize;
        let d = s.get(p + 5..(p + 5 + il).min(end)).unwrap_or(&[]);
        out.push(PmtEntry { pid, stype, format: classify(stype, d) });
        p += 5 + il;
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Audio framing

/// What a frame header says.
struct Frame {
    /// Bytes of the frame including its header.
    len: usize,
    /// Header bytes to leave out of the sample.
    strip: usize,
    /// PCM samples per channel the frame holds.
    samples: u32,
    rate: u32,
    channels: u16,
    /// Format specific: ADTS profile + channel config, AC-3 `dac3`, MPEG layer.
    info: u32,
}

enum Parse {
    Frame(Frame),
    /// Not enough bytes to tell.
    More,
    /// No frame starts here.
    Bad,
}

const ADTS_RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];

fn parse_adts(b: &[u8]) -> Parse {
    if b.len() < 7 {
        return if b.first().is_some_and(|&x| x != 0xFF) { Parse::Bad } else { Parse::More };
    }
    if b[0] != 0xFF || b[1] & 0xF6 != 0xF0 {
        return Parse::Bad;
    }
    let sf = ((b[2] >> 2) & 0xf) as usize;
    if sf >= 13 {
        return Parse::Bad;
    }
    let len = (((b[3] & 3) as usize) << 11) | ((b[4] as usize) << 3) | (b[5] >> 5) as usize;
    let strip = if b[1] & 1 == 1 { 7 } else { 9 };
    if len < strip {
        return Parse::Bad;
    }
    let chan = ((b[2] & 1) << 2) | (b[3] >> 6);
    let blocks = (b[6] & 3) as u32 + 1;
    let aot = ((b[2] >> 6) & 3) + 1;
    Parse::Frame(Frame { len, strip, samples: 1024 * blocks, rate: ADTS_RATES[sf], channels: if chan == 7 { 8 } else { chan as u16 }, info: (aot as u32) << 16 | (sf as u32) << 8 | chan as u32 })
}

const AC3_WORDS_48: [u16; 19] = [64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 640, 768, 896, 1024, 1152, 1280];
const AC3_WORDS_44: [u16; 19] = [69, 87, 104, 121, 139, 174, 208, 243, 278, 348, 417, 487, 557, 696, 835, 975, 1114, 1253, 1393];
const AC3_WORDS_32: [u16; 19] = [96, 120, 144, 168, 192, 240, 288, 336, 384, 480, 576, 672, 768, 960, 1152, 1344, 1536, 1728, 1920];
const AC3_CHANNELS: [u16; 8] = [2, 1, 2, 3, 3, 4, 4, 5];

fn parse_ac3(b: &[u8]) -> Parse {
    if b.len() < 8 {
        return if b.first().is_some_and(|&x| x != 0x0B) || b.get(1).is_some_and(|&x| x != 0x77) { Parse::Bad } else { Parse::More };
    }
    if b[0] != 0x0B || b[1] != 0x77 {
        return Parse::Bad;
    }
    let bsid = b[5] >> 3;
    let fscod = (b[4] >> 6) as usize;
    if bsid > 10 {
        // E-AC-3
        let len = ((((b[2] & 7) as usize) << 8 | b[3] as usize) + 1) * 2;
        let (rate, blocks) = if fscod == 3 {
            let r2 = (b[4] >> 4) & 3;
            if r2 == 3 {
                return Parse::Bad;
            }
            ([24000, 22050, 16000][r2 as usize], 6)
        } else {
            ([48000, 44100, 32000][fscod], [1u32, 2, 3, 6][((b[4] >> 4) & 3) as usize])
        };
        let acmod = ((b[4] >> 1) & 7) as usize;
        let lfe = (b[4] & 1) as u16;
        return Parse::Frame(Frame { len, strip: 0, samples: 256 * blocks, rate, channels: AC3_CHANNELS[acmod] + lfe, info: 0 });
    }
    if fscod == 3 {
        return Parse::Bad;
    }
    let code = (b[4] & 0x3f) as usize;
    if code >= 38 {
        return Parse::Bad;
    }
    let words = match fscod {
        0 => AC3_WORDS_48[code / 2],
        1 => AC3_WORDS_44[code / 2] + (code & 1) as u16,
        _ => AC3_WORDS_32[code / 2],
    } as usize;
    let mut r = BitReader::new(&b[6..]);
    let acmod = r.bits(3).unwrap_or(0);
    if acmod & 1 != 0 && acmod != 1 {
        let _ = r.bits(2);
    }
    if acmod & 4 != 0 {
        let _ = r.bits(2);
    }
    if acmod == 2 {
        let _ = r.bits(2);
    }
    let lfe = r.bits(1).unwrap_or(0);
    let bsmod = b[5] & 7;
    // dac3: fscod(2) bsid(5) bsmod(3) acmod(3) lfeon(1) bit_rate_code(5) reserved(5)
    let dac3 = (fscod as u32) << 22 | (bsid as u32) << 17 | (bsmod as u32) << 14 | acmod << 11 | lfe << 10 | ((code / 2) as u32) << 5;
    Parse::Frame(Frame { len: words * 2, strip: 0, samples: 1536, rate: [48000, 44100, 32000][fscod], channels: AC3_CHANNELS[acmod as usize] + lfe as u16, info: dac3 })
}

fn parse_mpa(b: &[u8]) -> Parse {
    if b.len() < 4 {
        return if b.first().is_some_and(|&x| x != 0xFF) { Parse::Bad } else { Parse::More };
    }
    if b[0] != 0xFF || b[1] & 0xE0 != 0xE0 {
        return Parse::Bad;
    }
    let version = (b[1] >> 3) & 3;
    let layer = (b[1] >> 1) & 3;
    let br = (b[2] >> 4) as usize;
    let sr = ((b[2] >> 2) & 3) as usize;
    if version == 1 || layer == 0 || br == 0 || br == 15 || sr == 3 {
        return Parse::Bad;
    }
    const V1: [[u32; 14]; 3] = [
        [32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448],
        [32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384],
        [32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320],
    ];
    const V2: [[u32; 14]; 2] = [[32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256], [8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160]];
    let l = 3 - layer as usize; // 0 = layer I
    let kbps = if version == 3 { V1[l][br - 1] } else { V2[(l > 0) as usize][br - 1] };
    let rate = [[11025, 12000, 8000], [0; 3], [22050, 24000, 16000], [44100, 48000, 32000]][version as usize][sr];
    let pad = ((b[2] >> 1) & 1) as u32;
    let (len, samples) = match (l, version) {
        (0, _) => ((12 * kbps * 1000 / rate + pad) * 4, 384),
        (1, _) => (144 * kbps * 1000 / rate + pad, 1152),
        (_, 3) => (144 * kbps * 1000 / rate + pad, 1152),
        _ => (72 * kbps * 1000 / rate + pad, 576),
    };
    Parse::Frame(Frame { len: len as usize, strip: 0, samples, rate, channels: if b[3] >> 6 == 3 { 1 } else { 2 }, info: layer as u32 })
}

// ---------------------------------------------------------------------------------------------
// Indexing

#[derive(Default)]
pub(crate) struct Params {
    pub(crate) vps: Option<Vec<u8>>,
    pub(crate) sps: Option<Vec<u8>>,
    pub(crate) pps: Option<Vec<u8>>,
}

pub(crate) struct AuInfo {
    /// Bytes after conversion to length prefixed form.
    pub(crate) size: u32,
    pub(crate) key: bool,
}

/// Looks through one Annex B access unit: converted size, whether decoding can start here, and
/// the first parameter sets seen.
pub(crate) fn scan_au(au: &[u8], hevc: bool, params: &mut Params) -> AuInfo {
    let (mut size, mut key) = (0u32, false);
    for u in nal::annexb_units(au) {
        if nal::annexb_dropped(u, hevc) {
            continue;
        }
        size += 4 + u.len() as u32;
        if hevc {
            let t = (u[0] >> 1) & 0x3f;
            let layer0 = u.len() > 1 && (u[0] & 1) == 0 && (u[1] >> 3) == 0;
            match t {
                16..=23 if layer0 => key = true,
                32 if layer0 && params.vps.is_none() => params.vps = Some(u.to_vec()),
                33 if layer0 && params.sps.is_none() => params.sps = Some(u.to_vec()),
                34 if layer0 && params.pps.is_none() => params.pps = Some(u.to_vec()),
                _ => {}
            }
        } else {
            match u[0] & 0x1f {
                5 => key = true,
                6 => key |= sei_has_recovery_point(&u[1..]),
                7 if params.sps.is_none() => params.sps = Some(u.to_vec()),
                8 if params.pps.is_none() => params.pps = Some(u.to_vec()),
                _ => {}
            }
        }
    }
    AuInfo { size, key }
}

fn sei_has_recovery_point(mut b: &[u8]) -> bool {
    while b.len() > 2 {
        let mut t = 0usize;
        while b.first() == Some(&0xFF) {
            t += 255;
            b = &b[1..];
        }
        let Some((&x, rest)) = b.split_first() else { return false };
        t += x as usize;
        b = rest;
        let mut n = 0usize;
        while b.first() == Some(&0xFF) {
            n += 255;
            b = &b[1..];
        }
        let Some((&x, rest)) = b.split_first() else { return false };
        n += x as usize;
        b = rest;
        if t == 6 {
            return true;
        }
        if n > b.len() {
            return false;
        }
        b = &b[n..];
    }
    false
}

/// Picture size and `avcC` from the first SPS / PPS.
pub(crate) fn avcc_from(p: &Params) -> Option<(Vec<u8>, u32, u32)> {
    let (sps, pps) = (p.sps.as_ref()?, p.pps.as_ref()?);
    if sps.len() < 4 {
        return None;
    }
    let rbsp = nal::unescape(&sps[1..]);
    let s = h264::sps(&mut BitReader::new(&rbsp)).ok()?;
    let (w, h) = s.display_size();
    let mut c = vec![1, sps[1], sps[2], sps[3], 0xFF, 0xE1];
    c.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    c.extend_from_slice(sps);
    c.push(1);
    c.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    c.extend_from_slice(pps);
    Some((c, w, h))
}

/// Picture size and `hvcC` from the first VPS / SPS / PPS.
pub(crate) fn hvcc_from(p: &Params) -> Option<(Vec<u8>, u32, u32)> {
    let (sps, pps) = (p.sps.as_ref()?, p.pps.as_ref()?);
    let rbsp = nal::unescape(sps);
    let mut b = BitReader::new(&rbsp);
    hevc::nal_header(&mut b).ok()?;
    let s = hevc::sps(&mut b).ok()?;
    let (w, h) = s.display_size();
    // profile_tier_level starts after the 2-byte NAL header and one byte (vps id, sub layers, nesting)
    let ptl = rbsp.get(3..15)?;
    let mut c = vec![1];
    c.extend_from_slice(&ptl[..11]); // profile space .. constraint flags
    c.push(ptl[11]); // level
    c.extend_from_slice(&[0xF0, 0, 0xFC, 0xFC | s.chroma_format_idc, 0xF8 | (s.bit_depth_luma - 8), 0xF8 | (s.bit_depth_chroma - 8), 0, 0, 0x03]);
    let arrays: Vec<(u8, &Vec<u8>)> = [(32u8, p.vps.as_ref()), (33, Some(sps)), (34, Some(pps))].into_iter().filter_map(|(t, u)| u.map(|u| (t, u))).collect();
    c.push(arrays.len() as u8);
    for (t, u) in arrays {
        c.push(0x80 | t);
        c.extend_from_slice(&1u16.to_be_bytes());
        c.extend_from_slice(&(u.len() as u16).to_be_bytes());
        c.extend_from_slice(u);
    }
    Some((c, w, h))
}

/// An audio stream cut into frames: the bytes not yet framed, with where each packet's payload
/// came from.
#[derive(Default)]
struct FrameBuf {
    buf: Vec<u8>,
    /// Consumed bytes at the front of `buf`.
    pos: usize,
    /// Stream position of `buf[0]`.
    base: u64,
    /// (stream position of a packet's payload, file offset of the packet's slot), ascending.
    chunks: Vec<(u64, u64)>,
    /// (stream position where a PES payload starts, its PTS).
    marks: VecDeque<(u64, i64)>,
}

impl FrameBuf {
    fn total(&self) -> u64 {
        self.base + self.buf.len() as u64
    }

    fn push(&mut self, body: &[u8], slot: u64) {
        if !body.is_empty() {
            self.chunks.push((self.total(), slot));
            self.buf.extend_from_slice(body);
        }
    }

    /// Packet slot and bytes to skip in it for the stream position `at`.
    fn locate(&self, at: u64) -> (u64, u64) {
        let k = self.chunks.partition_point(|c| c.0 <= at).saturating_sub(1);
        let c = self.chunks[k];
        (c.1, at - c.0)
    }

    fn compact(&mut self) {
        if self.pos < (1 << 16) {
            return;
        }
        let at = self.base + self.pos as u64;
        let k = self.chunks.partition_point(|c| c.0 <= at).saturating_sub(1);
        self.chunks.drain(..k);
        self.buf.drain(..self.pos);
        self.base = at;
        self.pos = 0;
    }
}

pub(crate) struct Es {
    pid: u16,
    stype: u8,
    format: Format,
    /// A PES being collected (video, LPCM): file offset of its first packet, times, payload.
    open: bool,
    cur_off: u64,
    cur_pts: Option<i64>,
    cur_dts: Option<i64>,
    cur: Vec<u8>,
    /// PES header bytes still to drop (a header can run into the next packet).
    hdr_skip: usize,
    fb: FrameBuf,
    last_time: Option<i64>,
    samples: Vec<Sample>,
    skip: Vec<u32>,
    raw: Vec<u32>,
    params: Params,
    // audio facts from the first frame / header
    first_frame: Option<Frame>,
    /// Running presentation time of the next frame, in sample rate units.
    next_pts: Option<i64>,
    lpcm: Option<(u32, u16, u16)>,
    mpeg2_size: (u32, u32),
    lost: bool,
}

impl Es {
    pub(crate) fn new(e: &PmtEntry) -> Self {
        Self {
            pid: e.pid,
            stype: e.stype,
            format: e.format,
            open: false,
            cur_off: 0,
            cur_pts: None,
            cur_dts: None,
            cur: Vec::new(),
            hdr_skip: 0,
            fb: FrameBuf::default(),
            last_time: None,
            samples: Vec::new(),
            skip: Vec::new(),
            raw: Vec::new(),
            params: Params::default(),
            first_frame: None,
            next_pts: None,
            lpcm: None,
            mpeg2_size: (0, 0),
            lost: false,
        }
    }

    fn is_unit_based(&self) -> bool {
        matches!(self.format, Format::H264 | Format::Hevc | Format::Mpeg2Video | Format::Lpcm)
    }

    fn is_frame_based(&self) -> bool {
        matches!(self.format, Format::Adts | Format::Mpa | Format::Ac3 | Format::Eac3)
    }

    fn unwrap(&mut self, v: i64) -> i64 {
        let mut v = v;
        if let Some(l) = self.last_time {
            while v + (WRAP >> 1) < l {
                v += WRAP;
            }
            while v > l + (WRAP >> 1) {
                v -= WRAP;
            }
        }
        self.last_time = Some(v);
        v
    }

    fn packet(&mut self, slot: u64, pusi: bool, pay: &[u8]) {
        if self.samples.len() >= SAMPLES_MAX || !(self.is_unit_based() || self.is_frame_based()) {
            return;
        }
        let mut body = pay;
        if pusi {
            let Some((hlen, pts, dts)) = pes_header(pay) else { return };
            self.start_pes(slot, pts, dts);
            body = pay.get(hlen..).unwrap_or(&[]);
            self.hdr_skip = hlen.saturating_sub(pay.len());
        } else {
            if !self.open {
                return;
            }
            let k = self.hdr_skip.min(body.len());
            body = &body[k..];
            self.hdr_skip -= k;
        }
        self.feed(slot, body);
    }

    /// A whole PES (program streams): `slot` is where it starts, `body` its payload.
    pub(crate) fn pes(&mut self, slot: u64, pts: Option<i64>, dts: Option<i64>, body: &[u8]) {
        if self.samples.len() >= SAMPLES_MAX || !(self.is_unit_based() || self.is_frame_based()) {
            return;
        }
        self.start_pes(slot, pts, dts);
        self.hdr_skip = 0;
        self.feed(slot, body);
    }

    fn start_pes(&mut self, slot: u64, pts: Option<i64>, dts: Option<i64>) {
        self.finish_unit();
        let (pts_raw, dts_raw) = (pts, dts);
        let pts = pts_raw.map(|t| self.unwrap(t));
        // the DTS stays next to its PTS, whatever side of a wrap each is on
        let dts = match (dts_raw, pts_raw, pts) {
            (Some(d), Some(pr), Some(p)) => Some(p + wrap_signed(d - pr)),
            _ => None,
        };
        if self.is_unit_based() {
            self.open = true;
            self.cur_off = slot;
            self.cur_pts = pts;
            self.cur_dts = dts;
            self.cur.clear();
        } else {
            if let Some(p) = pts {
                let at = self.fb.total();
                self.fb.marks.push_back((at, p));
            }
            self.open = true;
        }
    }

    fn feed(&mut self, slot: u64, body: &[u8]) {
        if self.is_unit_based() {
            self.cur.extend_from_slice(body);
        } else {
            self.fb.push(body, slot);
            self.frames();
        }
    }

    pub(crate) fn finish_unit(&mut self) {
        if !self.is_unit_based() || !self.open {
            return;
        }
        self.open = false;
        if self.cur.is_empty() || self.samples.len() >= SAMPLES_MAX {
            return;
        }
        let pts = self.cur_pts.or(self.cur_dts).or(self.last_time).unwrap_or(0);
        let dts = self.cur_dts.unwrap_or(pts);
        let (skip, size, key, pts_out) = match self.format {
            Format::H264 | Format::Hevc => {
                let info = scan_au(&self.cur, self.format == Format::Hevc, &mut self.params);
                (0u32, info.size, info.key, pts)
            }
            Format::Mpeg2Video => {
                let mut key = false;
                let c = &self.cur;
                let mut i = 0;
                while i + 6 < c.len() {
                    if c[i] == 0 && c[i + 1] == 0 && c[i + 2] == 1 {
                        if c[i + 3] == 0 {
                            key = (c[i + 5] >> 3) & 7 == 1;
                            break;
                        }
                        if c[i + 3] == 0xB3 && self.mpeg2_size.0 == 0 {
                            self.mpeg2_size = (((c[i + 4] as u32) << 4) | (c[i + 5] >> 4) as u32, (((c[i + 5] & 0xf) as u32) << 8) | c[i + 6] as u32);
                        }
                    }
                    i += 1;
                }
                (0, self.cur.len() as u32, key, pts)
            }
            Format::Lpcm => {
                if self.cur.len() < 4 {
                    return;
                }
                if self.lpcm.is_none() {
                    let h = &self.cur;
                    let rate = match h[2] & 0xf {
                        1 => 48000,
                        4 => 96000,
                        5 => 192000,
                        _ => 0,
                    };
                    let ch = [0u16, 1, 0, 2, 3, 3, 4, 4, 5, 6, 7, 8][((h[2] >> 4) as usize).min(11)];
                    let bits = match h[3] >> 6 {
                        1 => 16,
                        2 => 24, // 20-bit audio is stored in 24-bit words
                        3 => 24,
                        _ => 0,
                    };
                    if rate == 0 || ch == 0 || bits == 0 {
                        self.lost = true;
                        return;
                    }
                    self.lpcm = Some((rate, ch, bits));
                }
                let rate = self.lpcm.map_or(48000, |l| l.0) as i64;
                let n = self.cur.len() as u32 - 4;
                (4, n, true, (pts * rate + 45000).div_euclid(90000))
            }
            _ => return,
        };
        let dts_out = if self.format == Format::Lpcm { pts_out } else { dts };
        self.samples.push(Sample { offset: self.cur_off, size, dts: dts_out, pts: pts_out, key });
        self.skip.push(skip);
        self.raw.push(self.cur.len() as u32 - skip);
        self.cur.clear();
    }

    /// Cuts the frames out of the stream bytes collected so far.
    fn frames(&mut self) {
        loop {
            let b = &self.fb.buf[self.fb.pos..];
            if b.is_empty() {
                break;
            }
            let parsed = match self.format {
                Format::Adts => parse_adts(b),
                Format::Ac3 | Format::Eac3 => parse_ac3(b),
                _ => parse_mpa(b),
            };
            match parsed {
                Parse::More => break,
                Parse::Bad => {
                    // hunt for the next possible header byte
                    let want = if matches!(self.format, Format::Ac3 | Format::Eac3) { 0x0B } else { 0xFF };
                    let skip = b[1..].iter().position(|&x| x == want).map_or(b.len(), |p| p + 1);
                    self.fb.pos += skip;
                }
                Parse::Frame(f) => {
                    if b.len() < f.len {
                        break;
                    }
                    let at = self.fb.base + self.fb.pos as u64;
                    let (slot, in_packet) = self.fb.locate(at);
                    // the frame's time: the PES that started at or before it, else the running time
                    let mut anchor = None;
                    while self.fb.marks.front().is_some_and(|m| m.0 <= at) {
                        anchor = self.fb.marks.pop_front().map(|m| m.1);
                    }
                    if let Some(p) = anchor {
                        let p = (p * f.rate as i64 + 45000).div_euclid(90000);
                        match self.next_pts {
                            Some(n) if (p - n).abs() <= f.samples as i64 / 2 => {}
                            _ => self.next_pts = Some(p),
                        }
                    }
                    if self.first_frame.is_none() {
                        self.first_frame = Some(Frame { len: f.len, strip: f.strip, samples: f.samples, rate: f.rate, channels: f.channels, info: f.info });
                    }
                    if let Some(pts) = self.next_pts {
                        if self.samples.len() < SAMPLES_MAX {
                            self.samples.push(Sample { offset: slot, size: (f.len - f.strip) as u32, dts: pts, pts, key: true });
                            self.skip.push((in_packet + f.strip as u64) as u32);
                            self.raw.push((f.len - f.strip) as u32);
                        }
                        self.next_pts = Some(pts + f.samples as i64);
                    }
                    self.fb.pos += f.len;
                }
            }
        }
        self.fb.compact();
    }
}

/// A difference of two 33-bit counters brought into one half wrap around 0.
fn wrap_signed(mut x: i64) -> i64 {
    while x < -(WRAP >> 1) {
        x += WRAP;
    }
    while x > WRAP >> 1 {
        x -= WRAP;
    }
    x
}

/// Reads the structure of a transport stream (`.ts`, `.m2ts`, `.mts`).
pub fn read<R: Read + Seek>(r: &mut R) -> Result<Movie> {
    r.seek(SeekFrom::Start(0))?;
    let mut head = [0u8; 1024];
    let n = read_full(r, &mut head)?;
    let packet = detect(&head[..n]).ok_or(Error::Invalid("not a transport stream"))?;
    r.seek(SeekFrom::Start(0))?;

    let mut pat = Section::default();
    let mut pmt_pids: Vec<u16> = Vec::new();
    let mut pmt_secs: HashMap<u16, Section> = HashMap::new();
    let mut chosen: Option<u16> = None;
    let mut es: Vec<Es> = Vec::new();
    let mut by_pid: HashMap<u16, usize> = HashMap::new();

    let mut block = vec![0u8; packet * BLOCK_PACKETS];
    let mut offset = 0u64;
    'file: loop {
        let n = read_full(r, &mut block)?;
        let whole = n / packet * packet;
        for (k, p) in block[..whole].chunks_exact(packet).enumerate() {
            let ts = &p[packet - 188..];
            if ts[0] != SYNC {
                log::warn!("transport stream: sync lost at byte {}, the rest is ignored", offset + (k * packet) as u64);
                break 'file;
            }
            let pid = pid_of(ts);
            if pid == 0x1FFF {
                continue;
            }
            let pusi = ts[1] & 0x40 != 0;
            let Some(pay) = payload(ts) else { continue };
            if let Some(&i) = by_pid.get(&pid) {
                es[i].packet(offset + (k * packet) as u64, pusi, pay);
            } else if pid == 0 {
                if let Some(s) = pat.push(pusi, pay) {
                    for pm in parse_pat(&s) {
                        if !pmt_pids.contains(&pm) {
                            pmt_pids.push(pm);
                        }
                    }
                }
            } else if chosen.is_none() && pmt_pids.contains(&pid) {
                if let Some(s) = pmt_secs.entry(pid).or_default().push(pusi, pay) {
                    let entries = parse_pmt(&s);
                    if entries.iter().any(|e| matches!(e.format, Format::H264 | Format::Hevc | Format::Mpeg2Video | Format::Adts | Format::Ac3 | Format::Mpa | Format::Lpcm | Format::Eac3)) {
                        chosen = Some(pid);
                        for e in &entries {
                            if !by_pid.contains_key(&e.pid) {
                                by_pid.insert(e.pid, es.len());
                                es.push(Es::new(e));
                            }
                        }
                    }
                }
            }
        }
        offset += whole as u64;
        if n < block.len() {
            break;
        }
    }
    for e in &mut es {
        e.finish_unit();
    }
    if es.is_empty() {
        return Err(Error::Invalid("transport stream without a program"));
    }

    let tracks = build_tracks(es, packet);
    Ok(finish_movie(tracks))
}

/// The tracks of the streams found, in order. `packet` is the transport packet size, 0 for a
/// program stream.
pub(crate) fn build_tracks(es: Vec<Es>, packet: usize) -> Vec<Track> {
    let mut tracks = Vec::new();
    let mut seen_video = false;
    let mut seen_audio = false;
    for e in es {
        let layout_for = |annexb: Option<bool>, e: &Es| Arc::new(Layout { packet, pid: e.pid, annexb, skip: e.skip.clone(), raw: e.raw.clone() });
        let mut t = Track {
            id: e.pid as u32,
            kind: Kind::Other,
            fourcc: *b"    ",
            codec: Codec::Other,
            timescale: 90000,
            width: 0,
            height: 0,
            rotation: 0,
            channels: 0,
            sample_rate: 0,
            samples: Vec::new(),
            colour: None,
            default: true,
            codec_delay_ns: 0,
            discard_padding_ns: 0,
            par: None,
            storage: Storage::Plain,
            meta: Default::default(),
        };
        match e.format {
            Format::H264 | Format::Hevc => {
                let hevc = e.format == Format::Hevc;
                t.kind = Kind::Video;
                t.fourcc = if hevc { *b"hvc1" } else { *b"avc1" };
                if let Some((c, w, h)) = if hevc { hvcc_from(&e.params) } else { avcc_from(&e.params) } {
                    t.codec = if hevc { Codec::Hevc { config: c } } else { Codec::H264 { config: c } };
                    t.width = w;
                    t.height = h;
                } else {
                    log::warn!("transport stream: no usable parameter sets on PID {}", e.pid);
                }
                t.samples = e.samples.clone();
                t.storage = Storage::Ts(layout_for(Some(hevc), &e));
            }
            Format::Mpeg2Video => {
                t.kind = Kind::Video;
                t.fourcc = *b"mpg2";
                (t.width, t.height) = e.mpeg2_size;
                t.samples = e.samples.clone();
                t.storage = Storage::Ts(layout_for(None, &e));
            }
            Format::Lpcm => {
                if let Some((rate, ch, bits)) = e.lpcm {
                    t.kind = Kind::Audio;
                    t.fourcc = *b"lpcm";
                    t.codec = Codec::Pcm { bits, float: false, big_endian: true };
                    t.timescale = rate;
                    t.sample_rate = rate;
                    t.channels = ch;
                    t.samples = e.samples.clone();
                    t.storage = Storage::Ts(layout_for(None, &e));
                } else {
                    t.kind = Kind::Audio;
                }
            }
            Format::Adts | Format::Ac3 | Format::Eac3 | Format::Mpa => {
                t.kind = Kind::Audio;
                if let Some(f) = &e.first_frame {
                    t.timescale = f.rate;
                    t.sample_rate = f.rate;
                    t.channels = f.channels;
                    (t.fourcc, t.codec) = match e.format {
                        Format::Adts => {
                            let (aot, sf, ch) = (f.info >> 16, (f.info >> 8) & 0xf, f.info & 0xf);
                            let asc = ((aot << 11) | (sf << 7) | (ch << 3)) as u16;
                            (*b"mp4a", Codec::Aac { config: asc.to_be_bytes().to_vec() })
                        }
                        Format::Ac3 => (*b"ac-3", Codec::Ac3 { config: f.info.to_be_bytes()[1..].to_vec() }),
                        Format::Eac3 => (*b"ec-3", Codec::Eac3 { config: Vec::new() }),
                        _ if f.info == 1 => (*b".mp3", Codec::Mp3),
                        _ => (*b"mp2 ", Codec::Other),
                    };
                    t.samples = e.samples.clone();
                    t.storage = Storage::Ts(layout_for(None, &e));
                }
            }
            Format::Latm => {
                t.kind = Kind::Audio;
                t.fourcc = *b"latm";
            }
            Format::Other(name) => {
                t.kind = if matches!(name, "DTS" | "TrueHD") { Kind::Audio } else { Kind::Other };
                log::debug!("transport stream: PID {} ({}) is listed, not read", e.pid, name);
            }
        }
        let first_of_kind = match t.kind {
            Kind::Video => !std::mem::replace(&mut seen_video, true),
            Kind::Audio => !std::mem::replace(&mut seen_audio, true),
            Kind::Other => true,
        };
        t.default = first_of_kind;
        let _ = e.stype;
        tracks.push(t);
    }
    tracks
}

/// The movie of `tracks`: the earliest presentation time of any stream becomes 0.
pub(crate) fn finish_movie(mut tracks: Vec<Track>) -> Movie {
    let to90 = |t: &Track, v: i64| (v as i128 * 90000 / t.timescale.max(1) as i128) as i64;
    let base = tracks.iter().filter_map(|t| t.samples.iter().map(|s| to90(t, s.pts)).min()).min().unwrap_or(0);
    let mut dur = 0i64;
    for t in &mut tracks {
        let b = (base as i128 * t.timescale as i128 / 90000) as i64;
        for s in &mut t.samples {
            s.pts -= b;
            s.dts -= b;
        }
        if let Some(l) = t.samples.iter().map(|s| s.pts).max() {
            dur = dur.max(to90(t, l));
        }
    }
    // the last picture / frame lasts one more step
    for t in &tracks {
        if t.samples.len() > 1 {
            let step = if t.kind == Kind::Video {
                let mut p: Vec<i64> = t.samples.iter().map(|s| s.pts).collect();
                p.sort_unstable();
                p.windows(2).map(|w| w[1] - w[0]).filter(|&d| d > 0).min().unwrap_or(0)
            } else {
                t.samples[1].pts - t.samples[0].pts
            };
            let end = t.samples.iter().map(|s| s.pts).max().unwrap_or(0) + step;
            dur = dur.max(to90(t, end));
        }
    }
    Movie { timescale: 90000, duration: dur.max(0) as u64, tracks }
}
