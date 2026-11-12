//! MPEG program streams: `.mpg`, `.vob` (DVD), MPEG-1 system streams (VCD).
//!
//! [`read`] walks the pack headers and PES packets once. What it makes of the streams:
//! - MPEG audio (stream ids `0xC0..0xDF`): frames, like in a transport stream ([`crate::ts`]);
//! - AC-3 in private stream 1 (substream `0x80..0x87`, the 4-byte private header cut): frames;
//! - MPEG-1 / MPEG-2 video (`0xE0..0xEF`): the track is listed with its picture size from the
//!   first sequence header, **without samples**: nothing here decodes MPEG-2 video yet, and its
//!   pictures do not line up with PES packets, so a sample table would need a picture splitter
//!   first;
//! - everything else (DVD subpictures, LPCM, DTS, navigation packets) is ignored.
//!
//! A sample is read by walking the PES packets from the one it starts in
//! ([`crate::ts::Layout`] with a packet size of 0). H.264 / HEVC in a program stream (from a
//! PSM) is not handled.

use std::collections::HashMap;
use std::io::{BufReader, Read, Seek, SeekFrom};

use crate::mp4::{Codec, Kind, Movie, Sample, Storage, Track};
use crate::ts::{Es, Format, PmtEntry, build_tracks, finish_movie};
use crate::{Error, Result};

pub fn is_ps(head: &[u8]) -> bool {
    head.len() >= 4 && head[..4] == [0, 0, 1, 0xBA]
}

fn time(b: &[u8]) -> i64 {
    (((b[0] >> 1) & 7) as i64) << 30 | (b[1] as i64) << 22 | ((b[2] >> 1) as i64) << 15 | (b[3] as i64) << 7 | (b[4] >> 1) as i64
}

/// The header of a PES packet in `pkt` (starting `00 00 01 sid length`), MPEG-2 or MPEG-1 style:
/// bytes up to the payload, PTS, DTS.
pub(crate) fn pes_header(pkt: &[u8]) -> Option<(usize, Option<i64>, Option<i64>)> {
    if pkt.len() < 7 || pkt[..3] != [0, 0, 1] {
        return None;
    }
    if pkt[6] & 0xC0 == 0x80 {
        return crate::ts::pes_header(pkt);
    }
    // MPEG-1: stuffing, optional STD buffer size, then PTS / PTS+DTS or a lone 0x0F
    let mut p = 6;
    while pkt.get(p) == Some(&0xFF) {
        p += 1;
    }
    if pkt.get(p).is_some_and(|b| b & 0xC0 == 0x40) {
        p += 2;
    }
    let b = *pkt.get(p)?;
    match b & 0xF0 {
        0x20 if pkt.len() >= p + 5 => Some((p + 5, Some(time(&pkt[p..])), Some(time(&pkt[p..])))),
        0x30 if pkt.len() >= p + 10 => Some((p + 10, Some(time(&pkt[p..])), Some(time(&pkt[p + 5..])))),
        _ => Some((p + 1, None, None)),
    }
}

/// Picture size from the first MPEG video sequence header in `b`, and whether an extension
/// follows it (MPEG-2).
fn sequence_header(b: &[u8]) -> Option<(u32, u32, bool, u8)> {
    let mut i = 0;
    while i + 12 < b.len() {
        if b[i..i + 4] == [0, 0, 1, 0xB3] {
            let w = ((b[i + 4] as u32) << 4) | (b[i + 5] >> 4) as u32;
            let h = (((b[i + 5] & 0xf) as u32) << 8) | b[i + 6] as u32;
            let mpeg2 = b[i + 7..].windows(4).take(120).any(|x| x == [0, 0, 1, 0xB5]);
            return Some((w, h, mpeg2, b[i + 7] & 0xf));
        }
        i += 1;
    }
    None
}

/// A video stream being read: the picture splitter runs over the PES payloads.
struct VideoState {
    w: u32,
    h: u32,
    mpeg2: bool,
    rate_code: u8,
    split: crate::mpeg2::Splitter,
    /// Bytes of the elementary stream so far.
    es_pos: u64,
    /// (ES position of a PES payload, file offset of the PES packet).
    map: Vec<(u64, u64)>,
}

enum Stream {
    Audio(usize),
    Video(Box<VideoState>),
}

/// Samples of a video stream: one per picture, with times from the PES packets where there are
/// any, else counted from the temporal references within the GOP. Returns the samples and
/// the layout of how to read them.
fn video_samples(v: &mut VideoState, key: u16) -> (Vec<Sample>, std::sync::Arc<crate::ts::Layout>) {
    use crate::mpeg2::PicType;
    let units = v.split.finish();
    let times = crate::mpeg2::sample_times(&units, v.rate_code);
    let mut samples = Vec::with_capacity(units.len());
    let (mut skip, mut raw) = (Vec::new(), Vec::new());
    for ((u, size), (pts, dts)) in units.iter().zip(times) {
        let k = v.map.partition_point(|m| m.0 <= u.start).saturating_sub(1);
        let (es0, off) = v.map.get(k).copied().unwrap_or((0, 0));
        samples.push(Sample { offset: off, size: *size as u32, dts, pts, key: u.coding_type == PicType::I });
        skip.push((u.start - es0) as u32);
        raw.push(*size as u32);
    }
    (samples, std::sync::Arc::new(crate::ts::Layout { packet: 0, pid: key, annexb: None, skip, raw }))
}

/// Reads the structure of a program stream.
pub fn read<R: Read + Seek>(r: &mut R) -> Result<Movie> {
    let len = r.seek(SeekFrom::End(0))?;
    r.seek(SeekFrom::Start(0))?;
    let mut br = BufReader::with_capacity(1 << 20, r);
    let mut pos = 0u64;
    let mut es: Vec<Es> = Vec::new();
    let mut streams: HashMap<u16, Stream> = HashMap::new();
    let mut video_order: Vec<u16> = Vec::new();
    // the sync: a start code at `pos`; when something else is there, hunt for the next one
    let mut code = [0u8; 4];
    'file: while pos + 4 <= len {
        if br.read_exact(&mut code).is_err() {
            break;
        }
        if code[..3] != [0, 0, 1] {
            // resync byte by byte
            let mut w = code;
            loop {
                let mut b = [0u8; 1];
                pos += 1;
                if br.read_exact(&mut b).is_err() {
                    break 'file;
                }
                w = [w[1], w[2], w[3], b[0]];
                if w[..3] == [0, 0, 1] {
                    code = w;
                    break;
                }
            }
        }
        let start = pos;
        pos += 4;
        let id = code[3];
        match id {
            0xBA => {
                let mut h = [0u8; 10];
                if br.read_exact(&mut h[..8]).is_err() {
                    break;
                }
                pos += 8;
                if h[0] >> 6 == 1 {
                    if br.read_exact(&mut h[8..10]).is_err() {
                        break;
                    }
                    pos += 2;
                    let stuffing = (h[9] & 7) as i64;
                    br.seek_relative(stuffing)?;
                    pos += stuffing as u64;
                }
            }
            0xB9 => {}
            0xBB..=0xBF | 0xC0..=0xEF | 0xF0..=0xFF => {
                let mut l = [0u8; 2];
                if br.read_exact(&mut l).is_err() {
                    break;
                }
                let plen = u16::from_be_bytes(l) as usize;
                pos += 2;
                let wanted = id == 0xBD || (0xC0..=0xEF).contains(&id);
                if !wanted || pos + plen as u64 > len {
                    br.seek_relative(plen as i64)?;
                    pos += plen as u64;
                    if pos > len {
                        break;
                    }
                    continue;
                }
                let mut pkt = vec![0u8; 6 + plen];
                pkt[..4].copy_from_slice(&code);
                pkt[4..6].copy_from_slice(&l);
                br.read_exact(&mut pkt[6..])?;
                pos += plen as u64;
                let Some((hl, pts, dts)) = pes_header(&pkt) else { continue };
                let body = pkt.get(hl..).unwrap_or(&[]);
                // which stream: the id, plus the substream for private stream 1
                let (key, strip) = if id == 0xBD {
                    match body.first() {
                        Some(&sub) if (0x80..=0x87).contains(&sub) => (id as u16 | (sub as u16) << 8, 4),
                        _ => continue,
                    }
                } else {
                    (id as u16, 0)
                };
                if !streams.contains_key(&key) {
                    let kind = if id >= 0xE0 {
                        video_order.push(key);
                        Stream::Video(Box::new(VideoState { w: 0, h: 0, mpeg2: false, rate_code: 0, split: crate::mpeg2::Splitter::new(), es_pos: 0, map: Vec::new() }))
                    } else {
                        let format = if id == 0xBD { Format::Ac3 } else { Format::Mpa };
                        es.push(Es::new(&PmtEntry { pid: key, stype: 0, format }));
                        Stream::Audio(es.len() - 1)
                    };
                    streams.insert(key, kind);
                }
                match streams.get_mut(&key) {
                    Some(Stream::Audio(i)) => es[*i].pes(start, pts, dts, body.get(strip..).unwrap_or(&[])),
                    Some(Stream::Video(v)) => {
                        if v.w == 0 {
                            if let Some((sw, sh, m2, rc)) = sequence_header(body) {
                                (v.w, v.h, v.mpeg2, v.rate_code) = (sw, sh, m2, rc);
                            }
                        }
                        v.map.push((v.es_pos, start));
                        v.split.push(v.es_pos, body, pts, dts);
                        v.es_pos += body.len() as u64;
                    }
                    _ => {}
                }
                continue;
            }
            _ => {}
        }
    }
    for e in &mut es {
        e.finish_unit();
    }
    let mut tracks = build_tracks(es, 0);
    for key in video_order {
        let Some(Stream::Video(v)) = streams.get_mut(&key) else { continue };
        let (samples, layout) = video_samples(v, key);
        tracks.insert(
            0,
            Track {
                id: key as u32,
                kind: Kind::Video,
                fourcc: if v.mpeg2 { *b"mpg2" } else { *b"mpg1" },
                codec: Codec::Other,
                timescale: 90000,
                width: v.w,
                height: v.h,
                rotation: 0,
                channels: 0,
                sample_rate: 0,
                samples,
                colour: None,
                default: true,
                codec_delay_ns: 0,
                discard_padding_ns: 0,
                par: None,
                storage: Storage::Ts(layout),
                meta: Default::default(),
            },
        );
    }
    if tracks.is_empty() {
        return Err(Error::Invalid("program stream without audio or video"));
    }
    Ok(finish_movie(tracks))
}
