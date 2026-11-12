//! AVI (RIFF `AVI `), including OpenDML files over 1 GB.
//!
//! [`read`] walks `hdrl` (`avih`, one `strl` per stream with `strh` / `strf` and optionally an
//! OpenDML `indx`) and finds the chunks of every stream from, in this order: the OpenDML super
//! index and its `ix##` chunk indexes (files with several `RIFF AVIX` parts), the `idx1` index,
//! or, when a file has neither (a recording that was cut), a walk over the `movi` lists. Only
//! chunk headers are read, never the pictures, except for H.264 where the first bytes of each
//! chunk are looked at (see below).
//!
//! - Video times: `dwRate / dwScale` frames per second, the track's timescale is `dwRate` and
//!   frame `n` is at `n * dwScale`. A zero-sized chunk (a dropped frame) takes its time slot and
//!   is left out of the table.
//! - H.264 / HEVC (`H264`, `X264`, `avc1`, `HEVC`, ...): chunks hold Annex B access units; the
//!   track gets an `avcC` / `hvcC` made from the first in-band parameter sets and
//!   [`Storage::AnnexB`] converts on read. AVI has no presentation times: for H.264 with
//!   B-frames the display order is worked out from the picture order counts in the slice headers
//!   (POC type 0), so `pts` is the display slot and `dts` runs behind it by the reordering delay
//!   (like an MP4 edit list does). HEVC has no such reordering here (pts = dts).
//! - Motion JPEG and everything else video: listed, every frame a sync sample, not decoded.
//! - Audio: PCM (8/16/24/32 bit, float), MP3, AAC (raw with `AudioSpecificConfig`), AC-3 are given
//!   a codec; others (ADPCM, WMA, ...) are listed with [`Codec::Other`]. PCM chunks count
//!   `size / blockAlign` samples; compressed audio counts one `dwScale` per chunk.

use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;

use crate::bits::BitReader;
use crate::mp4::{AnnexB, Codec, Kind, Movie, Sample, Storage, Track};
use crate::ts::{Params, avcc_from, hvcc_from, scan_au};
use crate::{Error, Result, h264, nal};

const SAMPLES_MAX: usize = 50_000_000;
/// Bytes of a chunk looked at to find parameter sets and the first slice of an H.264 picture.
const PEEK: usize = 4096;

pub fn is_avi(head: &[u8]) -> bool {
    head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"AVI "
}

fn u16le(b: &[u8], o: usize) -> u16 {
    b.get(o..o + 2).map_or(0, |x| u16::from_le_bytes([x[0], x[1]]))
}
fn u32le(b: &[u8], o: usize) -> u32 {
    b.get(o..o + 4).map_or(0, |x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]))
}
fn u64le(b: &[u8], o: usize) -> u64 {
    b.get(o..o + 8).map_or(0, |x| u64::from_le_bytes(x.try_into().unwrap()))
}

/// A RIFF chunk header at `pos`: id, data size.
fn chunk_at<R: Read + Seek>(r: &mut R, pos: u64) -> Result<([u8; 4], u32)> {
    let mut h = [0u8; 8];
    r.seek(SeekFrom::Start(pos))?;
    r.read_exact(&mut h)?;
    Ok(([h[0], h[1], h[2], h[3]], u32le(&h, 4)))
}

fn read_at<R: Read + Seek>(r: &mut R, pos: u64, len: usize) -> Result<Vec<u8>> {
    let mut v = vec![0u8; len];
    r.seek(SeekFrom::Start(pos))?;
    r.read_exact(&mut v)?;
    Ok(v)
}

#[derive(Default)]
struct Stream {
    kind_tag: [u8; 4],
    handler: [u8; 4],
    scale: u32,
    rate: u32,
    sample_size: u32,
    /// `strf` as it is.
    format: Vec<u8>,
    /// The OpenDML `indx` chunk's data, if any.
    indx: Vec<u8>,
    /// (payload offset, size, key) in file order.
    chunks: Vec<(u64, u32, bool)>,
}

struct Movi {
    /// Position of the `movi` fourcc (idx1 offsets count from here).
    fourcc_pos: u64,
    /// First byte after the list.
    end: u64,
}

/// Walks the chunks inside `[pos, end)`, calling `f(reader, id, header position, size)` for
/// each. A `LIST` / `RIFF` is reported with its list type (`hdrl`, `movi`, ...) as the id; it is
/// not entered, `f` walks inside it when it wants to.
fn walk<R: Read + Seek>(r: &mut R, mut pos: u64, end: u64, f: &mut dyn FnMut(&mut R, [u8; 4], u64, u32) -> Result<()>) -> Result<()> {
    while pos + 8 <= end {
        let (id, size) = chunk_at(r, pos)?;
        let mut body_end = pos + 8 + size as u64;
        if &id == b"LIST" || &id == b"RIFF" {
            let mut t = [0u8; 4];
            r.read_exact(&mut t)?;
            f(r, t, pos, size)?;
        } else {
            f(r, id, pos, size)?;
        }
        if body_end & 1 == 1 {
            body_end += 1;
        }
        if body_end <= pos {
            break;
        }
        pos = body_end;
    }
    Ok(())
}

fn parse_strl<R: Read + Seek>(r: &mut R, start: u64, end: u64) -> Result<Stream> {
    let mut s = Stream { scale: 1, rate: 1, ..Default::default() };
    walk(r, start, end, &mut |r, id, pos, size| {
        let body = pos + 8;
        match &id {
            b"strh" if size >= 48 => {
                let b = read_at(r, body, 48)?;
                s.kind_tag = [b[0], b[1], b[2], b[3]];
                s.handler = [b[4], b[5], b[6], b[7]];
                s.scale = u32le(&b, 20).max(1);
                s.rate = u32le(&b, 24).max(1);
                s.sample_size = u32le(&b, 44);
            }
            b"strf" => s.format = read_at(r, body, (size as usize).min(1 << 20))?,
            b"indx" => s.indx = read_at(r, body, (size as usize).min(64 << 20))?,
            _ => {}
        }
        Ok(())
    })?;
    Ok(s)
}

/// The chunk list of one OpenDML index (`ix##`, or an `indx` of type 1 written in place).
fn chunk_index(b: &[u8], out: &mut Vec<(u64, u32, bool)>) {
    // wLongsPerEntry(2) bIndexSubType(1) bIndexType(1) nEntriesInUse(4) dwChunkId(4) qwBaseOffset(8) reserved(4)
    let n = u32le(b, 4) as usize;
    let base = u64le(b, 12);
    let per = (u16le(b, 0) as usize).max(2) * 4;
    for k in 0..n {
        let o = 24 + k * per;
        if o + 8 > b.len() {
            break;
        }
        let off = u32le(b, o) as u64;
        let size = u32le(b, o + 4);
        out.push((base + off, size & 0x7FFF_FFFF, size & 0x8000_0000 == 0));
    }
}

fn fourcc_lower(f: [u8; 4]) -> [u8; 4] {
    f.map(|c| c.to_ascii_lowercase())
}

/// Reads the structure of an AVI file.
pub fn read<R: Read + Seek>(r: &mut R) -> Result<Movie> {
    let len = r.seek(SeekFrom::End(0))?;
    let (id, riff_size) = chunk_at(r, 0)?;
    if &id != b"RIFF" {
        return Err(Error::Invalid("not a RIFF file"));
    }
    let mut ty = [0u8; 4];
    r.read_exact(&mut ty)?;
    if &ty != b"AVI " {
        return Err(Error::Invalid("not an AVI file"));
    }
    let first_end = (8 + riff_size as u64).min(len);

    let mut streams: Vec<Stream> = Vec::new();
    let mut movis: Vec<Movi> = Vec::new();
    let mut idx1: Option<(u64, u32)> = None;
    let mut micro_per_frame = 0u32;
    let mut walk_err = None;
    // the first RIFF: hdrl, movi, idx1
    let res = walk(r, 12, first_end, &mut |r, id, pos, size| {
        match &id {
            b"hdrl" => {
                walk(r, pos + 12, pos + 8 + size as u64, &mut |r, id, pos, size| {
                    match &id {
                        b"avih" if size >= 4 => micro_per_frame = u32le(&read_at(r, pos + 8, 4)?, 0),
                        b"strl" => streams.push(parse_strl(r, pos + 12, pos + 8 + size as u64)?),
                        _ => {}
                    }
                    Ok(())
                })?;
            }
            b"movi" => movis.push(Movi { fourcc_pos: pos + 8, end: (pos + 8 + size as u64).min(len) }),
            b"idx1" => idx1 = Some((pos + 8, size)),
            _ => {}
        }
        Ok(())
    });
    if let Err(e) = res {
        walk_err = Some(e);
    }
    if streams.is_empty() {
        return Err(walk_err.unwrap_or(Error::Invalid("AVI without streams")));
    }
    // further RIFF parts (AVIX): their movi lists
    let mut pos = first_end + (first_end & 1);
    while pos + 12 <= len {
        let Ok((id, size)) = chunk_at(r, pos) else { break };
        if &id != b"RIFF" {
            break;
        }
        let end = (pos + 8 + size as u64).min(len);
        let _ = walk(r, pos + 12, end, &mut |_, id, p, size| {
            if &id == b"movi" {
                movis.push(Movi { fourcc_pos: p + 8, end: (p + 8 + size as u64).min(len) });
            }
            Ok(())
        });
        pos = end + (end & 1);
    }

    // chunks: OpenDML indexes where every stream has one, else idx1, else a walk over movi
    let have_super = streams.iter().all(|s| s.indx.len() >= 24 && s.indx[3] <= 1);
    if have_super {
        for s in &mut streams {
            let ix = std::mem::take(&mut s.indx);
            if ix[3] == 1 {
                chunk_index(&ix, &mut s.chunks);
                continue;
            }
            let n = u32le(&ix, 4) as usize;
            let per = (u16le(&ix, 0) as usize).max(4) * 4;
            for k in 0..n {
                let o = 24 + k * per;
                if o + 16 > ix.len() {
                    break;
                }
                let off = u64le(&ix, o);
                let Ok((_, size)) = chunk_at(r, off) else { continue };
                let Ok(b) = read_at(r, off + 8, (size as usize).min(256 << 20)) else { continue };
                chunk_index(&b, &mut s.chunks);
            }
        }
    } else if let Some((pos, size)) = idx1 {
        read_idx1(r, pos, size, &movis, &mut streams)?;
    } else {
        scan_movi(r, &movis, &mut streams)?;
    }

    // whatever an index claims has to lie inside the file (a cut recording's index may not)
    for s in &mut streams {
        s.chunks.retain(|c| c.0 + c.1 as u64 <= len);
    }

    let mut tracks = Vec::new();
    let (mut seen_v, mut seen_a) = (false, false);
    for (n, s) in streams.iter().enumerate() {
        let mut t = Track {
            id: n as u32,
            kind: Kind::Other,
            fourcc: *b"    ",
            codec: Codec::Other,
            timescale: s.rate,
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
        match &s.kind_tag {
            b"vids" => build_video(r, s, &mut t)?,
            b"auds" => build_audio(s, &mut t),
            _ => {}
        }
        t.default = match t.kind {
            Kind::Video => !std::mem::replace(&mut seen_v, true),
            Kind::Audio => !std::mem::replace(&mut seen_a, true),
            Kind::Other => true,
        };
        tracks.push(t);
    }
    let dur = tracks
        .iter()
        .filter(|t| t.timescale > 0)
        .filter_map(|t| {
            let end = t.samples.iter().map(|s| s.pts).max()?;
            let step = t.samples.windows(2).map(|w| w[1].pts - w[0].pts).filter(|&d| d > 0).min().unwrap_or(0);
            Some((end + step) as f64 / t.timescale as f64)
        })
        .fold(0.0, f64::max);
    Ok(Movie { timescale: 1000, duration: (dur * 1000.0) as u64, tracks })
}

/// `NNdc` / `NNwb`: the stream number and the two letters.
fn chunk_stream(id: &[u8; 4]) -> Option<(usize, [u8; 2])> {
    if !id[0].is_ascii_digit() || !id[1].is_ascii_digit() {
        return None;
    }
    Some((((id[0] - b'0') * 10 + (id[1] - b'0')) as usize, [id[2].to_ascii_lowercase(), id[3].to_ascii_lowercase()]))
}

fn wanted(kind_tag: &[u8; 4], suffix: [u8; 2]) -> bool {
    match kind_tag {
        b"vids" => matches!(&suffix, b"dc" | b"db"),
        b"auds" => &suffix == b"wb",
        _ => false,
    }
}

fn read_idx1<R: Read + Seek>(r: &mut R, pos: u64, size: u32, movis: &[Movi], streams: &mut [Stream]) -> Result<()> {
    let n = (size / 16) as usize;
    let data = read_at(r, pos, n * 16)?;
    let Some(first_movi) = movis.first() else { return Err(Error::Invalid("AVI without a movi list")) };
    // offsets count from the movi fourcc; some writers use absolute positions
    let mut base = first_movi.fourcc_pos;
    if n > 0 {
        let off = u32le(&data, 8) as u64;
        let id = &data[0..4];
        let mut ok = |b: u64| chunk_at(r, b + off).is_ok_and(|(i, _)| i == id);
        if !ok(base) && ok(0) {
            base = 0;
        }
    }
    for e in data.chunks_exact(16) {
        let id = [e[0], e[1], e[2], e[3]];
        let Some((sn, suffix)) = chunk_stream(&id) else { continue };
        let Some(s) = streams.get_mut(sn) else { continue };
        if !wanted(&s.kind_tag, suffix) {
            continue;
        }
        let flags = u32le(e, 4);
        let off = base + u32le(e, 8) as u64;
        s.chunks.push((off + 8, u32le(e, 12), flags & 0x10 != 0));
    }
    Ok(())
}

fn scan_movi<R: Read + Seek>(r: &mut R, movis: &[Movi], streams: &mut [Stream]) -> Result<()> {
    for m in movis {
        scan_list(r, m.fourcc_pos + 4, m.end, streams)?;
    }
    Ok(())
}

fn scan_list<R: Read + Seek>(r: &mut R, start: u64, end: u64, streams: &mut [Stream]) -> Result<()> {
    let mut pos = start;
    while pos + 8 <= end {
        let Ok((id, size)) = chunk_at(r, pos) else { break };
        let data = pos + 8;
        if &id == b"LIST" {
            scan_list(r, data + 4, (data + size as u64).min(end), streams)?;
        } else if let Some((sn, suffix)) = chunk_stream(&id)
            && let Some(s) = streams.get_mut(sn)
            && wanted(&s.kind_tag, suffix)
            && data + size as u64 <= end
        {
            // no index: every audio chunk and every MJPEG frame is a sync sample; H.264 is
            // judged from the content later (see build_video)
            s.chunks.push((data, size, true));
        }
        pos = data + size as u64;
        pos += pos & 1;
    }
    Ok(())
}

fn build_video<R: Read + Seek>(r: &mut R, s: &Stream, t: &mut Track) -> Result<()> {
    t.kind = Kind::Video;
    t.timescale = s.rate;
    let f = &s.format;
    t.width = u32le(f, 4);
    t.height = (u32le(f, 8) as i32).unsigned_abs();
    let comp = if f.len() >= 20 { [f[16], f[17], f[18], f[19]] } else { s.handler };
    let comp = if comp == [0; 4] { s.handler } else { comp };
    t.fourcc = comp;
    let lower = fourcc_lower(comp);
    let h264 = matches!(&lower, b"h264" | b"x264" | b"avc1" | b"davc");
    let hevc = matches!(&lower, b"hevc" | b"hvc1" | b"h265" | b"x265");
    let step = s.scale as i64;
    let mut slot = 0i64;
    let mut samples = Vec::new();
    let mut raw = Vec::new();
    for &(off, size, key) in &s.chunks {
        if size > 0 && samples.len() < SAMPLES_MAX {
            samples.push(Sample { offset: off, size, dts: slot * step, pts: slot * step, key });
            raw.push(size);
        }
        slot += 1;
    }
    if !(h264 || hevc) || samples.is_empty() {
        t.samples = samples;
        return Ok(());
    }

    // Annex B chunks, or length prefixed ones with an avcC / hvcC in the format's extra data
    let extra = f.get(40..).unwrap_or(&[]);
    let head = read_at(r, samples[0].offset, (raw[0] as usize).min(8))?;
    let annexb = head.starts_with(&[0, 0, 1]) || head.starts_with(&[0, 0, 0, 1]);
    if !annexb {
        if nal::avcc(extra).is_ok() || nal::hvcc(extra).is_ok() {
            t.codec = if hevc { Codec::Hevc { config: extra.to_vec() } } else { Codec::H264 { config: extra.to_vec() } };
            t.samples = samples;
        }
        return Ok(());
    }
    // every picture is read: its length prefixed size is not known otherwise (and without an
    // index, or with one that calls everything a sync sample, the pictures say what is a sync sample)
    let from_content = samples.iter().all(|s| s.key);
    let mut params = Params::default();
    let mut any_key = false;
    for (i, smp) in samples.iter_mut().enumerate() {
        let data = read_at(r, smp.offset, raw[i] as usize)?;
        let info = scan_au(&data, hevc, &mut params);
        smp.size = info.size;
        if from_content {
            smp.key = info.key;
            any_key |= info.key;
        }
    }
    if from_content && !any_key {
        samples.iter_mut().for_each(|s| s.key = true);
    }
    let made = if hevc { hvcc_from(&params) } else { avcc_from(&params) };
    let Some((config, w, h)) = made else {
        log::warn!("AVI: no parameter sets found in the pictures");
        t.samples = samples;
        return Ok(());
    };
    t.codec = if hevc { Codec::Hevc { config } } else { Codec::H264 { config } };
    if t.width == 0 || t.height == 0 {
        (t.width, t.height) = (w, h);
    }
    if h264 {
        reorder_h264(r, &mut samples, &raw, step);
    }
    t.samples = samples;
    t.storage = Storage::AnnexB(Arc::new(AnnexB { hevc, raw }));
    Ok(())
}

/// AVI stores H.264 pictures in decoding order with no presentation times. Gives each sample
/// the display slot from its picture order count (POC type 0): `pts` is the slot, `dts` is the
/// decoding slot moved back by the largest reordering delay so that `dts <= pts`.
fn reorder_h264<R: Read + Seek>(r: &mut R, samples: &mut [Sample], raw: &[u32], step: i64) {
    let mut sps_map: Vec<Option<h264::Sps>> = vec![None; 32];
    let mut pps_map: Vec<Option<h264::Pps>> = vec![None; 256];
    let (mut prev_msb, mut prev_lsb) = (0i32, 0i32);
    let mut pocs: Vec<(usize, i32)> = Vec::with_capacity(samples.len()); // (period, poc)
    let mut period = 0usize;
    let mut started = false;
    for (i, smp) in samples.iter().enumerate() {
        let mut found = None;
        let mut want = raw[i].min(PEEK as u32) as usize;
        loop {
            let Ok(data) = read_at(r, smp.offset, want) else { return };
            for u in nal::annexb_units(&data) {
                let Some(ty) = h264::nal_unit_type(u) else { continue };
                match ty {
                    7 | 8 => {
                        let rbsp = nal::unescape(&u[1..]);
                        let mut b = BitReader::new(&rbsp);
                        if ty == 7 {
                            if let Ok(s) = h264::sps(&mut b) {
                                let id = s.id as usize;
                                sps_map[id] = Some(s);
                            }
                        } else if let Ok(p) = h264::pps(&mut b, |id| sps_map.get(id as usize).and_then(|s| s.as_ref()).map_or(1, |s| s.chroma_format_idc)) {
                            let id = p.id as usize;
                            pps_map[id] = Some(p);
                        }
                    }
                    1 | 5 if found.is_none() => {
                        let rbsp = nal::unescape(&u[1..u.len().min(256)]);
                        let mut b = BitReader::new(&rbsp);
                        let sh = h264::slice_header(&mut b, ty, h264::nal_ref_idc(u), |id| pps_map.get(id as usize).and_then(|p| p.as_ref()), |id| sps_map.get(id as usize).and_then(|s| s.as_ref()));
                        if let Ok(sh) = sh {
                            let sps = pps_map[sh.pps_id as usize].as_ref().and_then(|p| sps_map[p.sps_id as usize].as_ref());
                            found = sps.map(|s| (sh, s.poc_type, s.log2_max_poc_lsb));
                        }
                    }
                    _ => {}
                }
            }
            if found.is_some() || want >= raw[i] as usize {
                break;
            }
            want = raw[i] as usize;
        }
        let Some((sh, poc_type, log2_lsb)) = found else { return };
        if poc_type != 0 {
            return; // no reordering to undo (types 1 and 2 are for streams without B-frames or are rare)
        }
        let idr = sh.is_idr();
        if idr {
            period += 1;
            (prev_msb, prev_lsb) = (0, 0);
        } else if !started {
            period += 1;
        }
        started = true;
        let max_lsb = 1i32 << log2_lsb;
        let lsb = sh.poc_lsb as i32;
        let msb = if lsb < prev_lsb && prev_lsb - lsb >= max_lsb / 2 {
            prev_msb + max_lsb
        } else if lsb > prev_lsb && lsb - prev_lsb > max_lsb / 2 {
            prev_msb - max_lsb
        } else {
            prev_msb
        };
        let top = msb + lsb;
        let poc = top.min(top + sh.delta_poc_bottom);
        if sh.nal_ref_idc != 0 {
            (prev_msb, prev_lsb) = (msb, lsb);
        }
        pocs.push((period, poc));
    }
    // rank within each period: the display slot
    let mut rank = vec![0usize; samples.len()];
    let mut i = 0;
    while i < pocs.len() {
        let mut j = i;
        while j < pocs.len() && pocs[j].0 == pocs[i].0 {
            j += 1;
        }
        let mut order: Vec<usize> = (i..j).collect();
        order.sort_by_key(|&k| pocs[k].1);
        for (n, &k) in order.iter().enumerate() {
            rank[k] = i + n;
        }
        i = j;
    }
    let delay = rank.iter().enumerate().map(|(k, &rk)| k as i64 - rk as i64).max().unwrap_or(0).max(0);
    for (k, s) in samples.iter_mut().enumerate() {
        s.pts = rank[k] as i64 * step;
        s.dts = (k as i64 - delay) * step;
    }
}

fn build_audio(s: &Stream, t: &mut Track) {
    t.kind = Kind::Audio;
    let f = &s.format;
    let mut tag = u16le(f, 0);
    t.channels = u16le(f, 2);
    t.sample_rate = u32le(f, 4);
    let block = u16le(f, 12).max(1) as u32;
    let bits = u16le(f, 14);
    let extra = f.get(18..).map(|e| &e[..(u16le(f, 16) as usize).min(e.len())]).unwrap_or(&[]);
    if tag == 0xFFFE && extra.len() >= 22 {
        tag = u16le(extra, 8);
    }
    t.fourcc = [b'[', (tag & 0xff) as u8, (tag >> 8) as u8, b']'];
    t.codec = match tag {
        1 if matches!(bits, 8 | 16 | 24 | 32) => Codec::Pcm { bits, float: false, big_endian: false },
        3 if matches!(bits, 32 | 64) => Codec::Pcm { bits, float: true, big_endian: false },
        0x55 => Codec::Mp3,
        0xFF | 0x00FF if !extra.is_empty() => Codec::Aac { config: extra.to_vec() },
        0x2000 => Codec::Ac3 { config: Vec::new() },
        _ => Codec::Other,
    };
    if matches!(t.codec, Codec::Other) {
        // listed, not read
        return;
    }
    // time: a chunk of PCM holds size / blockAlign sample frames of `scale` ticks, compressed
    // audio one `scale` per chunk
    let pcm = matches!(t.codec, Codec::Pcm { .. });
    let (mut tick, mut samples) = (0i64, Vec::with_capacity(s.chunks.len()));
    let per_sample = if s.sample_size > 0 { s.sample_size } else { block };
    for &(off, size, _) in &s.chunks {
        if size == 0 || samples.len() >= SAMPLES_MAX {
            continue;
        }
        samples.push(Sample { offset: off, size, dts: tick, pts: tick, key: true });
        tick += if pcm || s.sample_size > 0 { (size / per_sample.max(1)) as i64 * s.scale as i64 } else { s.scale as i64 };
    }
    // timescale: ticks per second is dwRate / dwScale, in units of dwScale; when the stream's
    // rate counts bytes (MP3 written with dwSampleSize 1) the times stay in those units
    t.timescale = s.rate;
    t.samples = samples;
}
