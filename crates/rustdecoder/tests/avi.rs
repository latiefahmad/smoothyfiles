//! The AVI reader against ffprobe, plus hand-built files for what ffmpeg's muxer does not write
//! at this size (OpenDML with several `RIFF AVIX` parts, files without an index).
//!
//! What is compared with `ffprobe -show_packets`:
//! - count, sync flag, size and CRC32 of every packet's chunk bytes (for Annex B H.264 the chunk
//!   as stored; `Sample::size` is the length prefixed size, whose NAL lengths are checked to
//!   fill the sample);
//! - decode times: ffprobe's `dts` (AVI has no `pts`) in the stream's time base equals the
//!   sample's `dts` and `pts` for streams without B-frames. With B-frames ffprobe shows only the
//!   chunk order; there `pts` has to be the display order (a dense run of slots, `dts <= pts`)
//!   and the GPU decoder's pictures are compared with ffmpeg's in display order.

mod common;

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use common::*;
use rustdecoder::mp4::{Codec, Kind, Storage, Track};

const H264: &[&str] = &["-c:v", "libx264", "-pix_fmt", "yuv420p", "-preset", "veryfast", "-x264-params", "keyint=30:bframes=0"];
const H264_B: &[&str] = &["-c:v", "libx264", "-pix_fmt", "yuv420p", "-preset", "veryfast", "-x264-params", "keyint=30:bframes=3"];
const AVI: &[&str] = &["-f", "avi"];

fn raw_bytes(f: &mut File, t: &Track, i: usize) -> Vec<u8> {
    let s = t.samples[i];
    let n = match &t.storage {
        Storage::AnnexB(a) => a.raw[i],
        _ => s.size,
    } as usize;
    let mut b = vec![0u8; n];
    f.seek(SeekFrom::Start(s.offset)).unwrap();
    f.read_exact(&mut b).unwrap();
    b
}

fn check_track(clip: &Path, t: &Track, stream: &str, times: bool) -> usize {
    let want = ffprobe_full(clip, stream);
    assert_eq!(t.samples.len(), want.len(), "{}: {stream} packet count", clip.display());
    let mut f = File::open(clip).unwrap();
    for (i, (s, w)) in t.samples.iter().zip(&want).enumerate() {
        assert_eq!(s.key, w.key, "{stream} packet {i}: key");
        let raw = raw_bytes(&mut f, t, i);
        assert_eq!(raw.len() as u32, w.size, "{stream} packet {i}: size");
        assert_eq!(crc32fast::hash(&raw), w.crc, "{stream} packet {i}: CRC32");
        if times {
            assert_eq!(s.dts, w.dts, "{stream} packet {i}: dts");
            assert_eq!(s.pts, w.dts, "{stream} packet {i}: pts");
        }
    }
    want.len()
}

fn check_nal_lengths(clip: &Path, t: &Track) {
    let mut f = File::open(clip).unwrap();
    let mut buf = Vec::new();
    for i in 0..t.samples.len() {
        t.read_sample(&mut f, i, &mut buf).unwrap();
        assert_eq!(buf.len() as u32, t.samples[i].size, "sample {i}: size");
        let mut p = 0usize;
        while p < buf.len() {
            let n = u32::from_be_bytes(buf[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + n;
        }
        assert_eq!(p, buf.len(), "sample {i}: NAL units do not fill the sample");
    }
}

fn open(clip: &Path) -> rustdecoder::mp4::Movie {
    rustdecoder::container::read(&mut File::open(clip).unwrap()).unwrap()
}

fn check_clip(name: &str, video: &[&str], audio: &[&str], a_ok: fn(&Codec) -> bool, a_fmt: (u32, u16), b_frames: bool) {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg not found");
        return;
    }
    let clip = make_av(name, video, audio, AVI);
    let movie = open(&clip);
    let v = movie.tracks.iter().find(|t| t.kind == Kind::Video).expect("video track");
    assert!(matches!(v.codec, Codec::H264 { .. }), "{name}: {:?}", v.codec);
    assert_eq!((v.width, v.height), (640, 360));
    let nv = check_track(&clip, v, "v:0", !b_frames);
    check_nal_lengths(&clip, v);
    if b_frames {
        let mut pts: Vec<i64> = v.samples.iter().map(|s| s.pts).collect();
        assert!(v.samples.iter().all(|s| s.dts <= s.pts), "dts after pts");
        pts.sort_unstable();
        let step = v.samples.iter().map(|s| s.dts).max().unwrap() / (v.samples.len() as i64 - 1).max(1);
        assert!(pts.windows(2).all(|w| w[1] - w[0] == step.max(1)), "display slots are not a dense run: {:?}", &pts[..10]);
        assert_eq!(pts[0], 0);
    }
    let a = movie.tracks.iter().find(|t| t.kind == Kind::Audio).expect("audio track");
    assert!(a_ok(&a.codec), "{name}: audio {:?}", a.codec);
    assert_eq!((a.sample_rate, a.channels), a_fmt);
    let na = check_track(&clip, a, "a:0", false);
    // audio times: the end of the last chunk is the length of the sound (3 s)
    let last = a.samples.last().unwrap();
    assert!(a.seconds(last.pts) <= 3.1 && a.seconds(last.pts) > 2.5, "{name}: last audio chunk at {}", a.seconds(last.pts));
    eprintln!("{name}: {nv} video + {na} audio packets match ffprobe (timescale v {} a {}), movie {:.2} s", v.timescale, a.timescale, movie.duration_secs());
}

#[test]
fn h264_pcm() {
    check_clip("a_h264_pcm.avi", H264, &["-c:a", "pcm_s16le"], |c| matches!(c, Codec::Pcm { bits: 16, float: false, big_endian: false }), (48000, 1), false);
}

#[test]
fn h264_mp3_with_b_frames() {
    check_clip("a_h264_mp3_b.avi", H264_B, &["-c:a", "libmp3lame", "-b:a", "128k"], |c| matches!(c, Codec::Mp3), (48000, 1), true);
}

#[test]
fn h264_aac() {
    check_clip("a_h264_aac.avi", H264, &["-c:a", "aac", "-b:a", "96k"], |c| matches!(c, Codec::Aac { .. }), (48000, 1), false);
}

#[test]
fn h264_ac3() {
    check_clip("a_h264_ac3.avi", H264, &["-c:a", "ac3", "-b:a", "192k"], |c| matches!(c, Codec::Ac3 { .. }), (48000, 1), false);
}

#[test]
fn mjpeg_is_listed() {
    if !have_ffmpeg() {
        return;
    }
    let clip = make_av("a_mjpeg_pcm.avi", &["-c:v", "mjpeg", "-pix_fmt", "yuvj420p"], &["-c:a", "pcm_s16le"], AVI);
    let movie = open(&clip);
    let v = movie.tracks.iter().find(|t| t.kind == Kind::Video).unwrap();
    assert_eq!(&v.fourcc, b"MJPG");
    assert!(matches!(v.codec, Codec::Other));
    assert_eq!((v.width, v.height), (640, 360));
    check_track(&clip, v, "v:0", true);
    assert!(v.samples.iter().all(|s| s.key));
}

// ---- hand-built files -------------------------------------------------------------------------

fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = id.to_vec();
    v.extend_from_slice(&(body.len() as u32).to_le_bytes());
    v.extend_from_slice(body);
    if body.len() % 2 == 1 {
        v.push(0);
    }
    v
}

fn list(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut b = kind.to_vec();
    b.extend_from_slice(body);
    chunk(b"LIST", &b)
}

fn le32(v: &[u32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// strl of a Motion JPEG video stream (25 fps) or 8-bit mono PCM audio (8 kHz), `indx` optional.
fn strl(video: bool, indx: Option<Vec<u8>>) -> Vec<u8> {
    let mut strh = Vec::new();
    strh.extend_from_slice(if video { b"vids" } else { b"auds" });
    strh.extend_from_slice(if video { b"MJPG" } else { &[0; 4] });
    // flags, priority+language, initialFrames, scale, rate, start, length, buffer, quality, sampleSize
    strh.extend_from_slice(&le32(&[0, 0, 0, 1, if video { 25 } else { 8000 }, 0, 10, 0, 0, if video { 0 } else { 1 }, 0, 0]));
    let mut strf = Vec::new();
    if video {
        strf.extend_from_slice(&le32(&[40, 64, 48, 0x0018_0001]));
        strf.extend_from_slice(b"MJPG");
        strf.extend_from_slice(&le32(&[0; 5]));
    } else {
        // PCM, 1 channel, 8000 Hz, 8000 B/s, block 1, 8 bits, no extra
        strf.extend_from_slice(&[1, 0, 1, 0]);
        strf.extend_from_slice(&le32(&[8000, 8000]));
        strf.extend_from_slice(&[1, 0, 8, 0, 0, 0]);
    }
    let mut body = chunk(b"strh", &strh);
    body.extend(chunk(b"strf", &strf));
    if let Some(i) = indx {
        body.extend(chunk(b"indx", &i));
    }
    list(b"strl", &body)
}

fn hdrl(indx: [Option<Vec<u8>>; 2]) -> Vec<u8> {
    let [iv, ia] = indx;
    let mut b = chunk(b"avih", &le32(&[40000, 0, 0, 0x10, 10, 0, 2, 0, 64, 48, 0, 0, 0, 0, 0]));
    b.extend(strl(true, iv));
    b.extend(strl(false, ia));
    list(b"hdrl", &b)
}

/// Frame `n` of the test movie: 5 + n bytes, so odd sizes get padded.
fn frame(n: usize) -> Vec<u8> {
    vec![(n as u8).wrapping_mul(7) | 1; 5 + n]
}
fn sound(n: usize) -> Vec<u8> {
    vec![0x80 + n as u8; 320]
}

/// Checks the movie holds frames `0..frames` and sound chunks `0..frames` with the right bytes.
fn check_synthetic(name: &str, bytes: &[u8], frames: usize, keys: &[usize]) {
    let path = temp_dir().join(format!("synthetic_{name}.avi"));
    std::fs::write(&path, bytes).unwrap();
    let movie = open(&path);
    let (v, a) = (&movie.tracks[0], &movie.tracks[1]);
    assert_eq!((v.samples.len(), a.samples.len()), (frames, frames));
    assert_eq!((v.width, v.height, v.timescale), (64, 48, 25));
    assert_eq!((a.sample_rate, a.channels, a.timescale), (8000, 1, 8000));
    let mut f = File::open(&path).unwrap();
    let mut buf = Vec::new();
    for n in 0..frames {
        v.read_sample(&mut f, n, &mut buf).unwrap();
        assert_eq!(buf, frame(n), "frame {n}");
        assert_eq!(v.samples[n].key, keys.contains(&n), "frame {n} sync");
        assert_eq!(v.samples[n].pts, n as i64);
        a.read_sample(&mut f, n, &mut buf).unwrap();
        assert_eq!(buf, sound(n), "sound {n}");
        // 320 bytes of 8 kHz mono PCM are 40 ms
        assert_eq!(a.samples[n].pts, 320 * n as i64);
    }
}

fn movi_with(range: std::ops::Range<usize>, nested: bool) -> Vec<u8> {
    let mut m = Vec::new();
    for n in range {
        let mut pair = chunk(b"00dc", &frame(n));
        pair.extend(chunk(b"01wb", &sound(n)));
        if nested && n % 2 == 0 {
            m.extend(list(b"rec ", &pair));
        } else {
            m.extend(pair);
        }
    }
    m
}

fn riff(kind: &[u8; 4], parts: &[Vec<u8>]) -> Vec<u8> {
    let mut b = kind.to_vec();
    parts.iter().for_each(|p| b.extend_from_slice(p));
    chunk(b"RIFF", &b)
}

#[test]
fn walk_without_an_index_and_with_rec_lists() {
    let movi = list(b"movi", &movi_with(0..8, true));
    let file = riff(b"AVI ", &[hdrl([None, None]), chunk(b"JUNK", &[0; 13]), movi]);
    // no idx1: everything is found by walking, every frame counts as a sync sample
    check_synthetic("walk", &file, 8, &(0..8).collect::<Vec<_>>());
}

#[test]
fn idx1_relative_and_absolute_offsets() {
    for absolute in [false, true] {
        let head = hdrl([None, None]);
        let movi_body = movi_with(0..6, false);
        // positions: RIFF(8) + "AVI "(4) + hdrl + LIST movi header(8) + "movi"(4)
        let movi_fourcc = 12 + head.len() + 8;
        let mut idx = Vec::new();
        let mut off = 4usize; // after the "movi" fourcc
        for n in 0..6 {
            for (id, len, key) in [(b"00dc", frame(n).len(), n % 3 == 0), (b"01wb", 320, true)] {
                let o = if absolute { movi_fourcc + off } else { off };
                idx.extend_from_slice(&le32(&[u32::from_le_bytes(*id), if key { 0x10 } else { 0 }, o as u32, len as u32]));
                off += 8 + len + len % 2;
            }
        }
        let file = riff(b"AVI ", &[head, list(b"movi", &movi_body), chunk(b"idx1", &idx)]);
        check_synthetic(if absolute { "idx1_abs" } else { "idx1_rel" }, &file, 6, &[0, 3]);
    }
}

#[test]
fn a_cut_file_keeps_its_whole_chunks() {
    let movi = list(b"movi", &movi_with(0..8, false));
    let mut file = riff(b"AVI ", &[hdrl([None, None]), movi]);
    // cut in the middle of the 7th frame's chunk: the RIFF size says more than the file has
    let cut = file.len() - (7 * (chunk(b"00dc", &frame(7)).len() + 8 + 320) / 2);
    file.truncate(cut);
    let path = temp_dir().join("synthetic_cut.avi");
    std::fs::write(&path, &file).unwrap();
    let movie = open(&path);
    let (v, a) = (movie.tracks[0].samples.len(), movie.tracks[1].samples.len());
    assert!((4..8).contains(&v) && (3..8).contains(&a) && v.abs_diff(a) <= 1, "{v} frames, {a} sound chunks");
    let mut f = File::open(&path).unwrap();
    let mut buf = Vec::new();
    for n in 0..v {
        movie.tracks[0].read_sample(&mut f, n, &mut buf).unwrap();
        assert_eq!(buf, frame(n));
    }
}

#[test]
fn opendml_super_index_over_two_riff_parts() {
    // part 1: frames 0..5, part 2 (AVIX): frames 5..10; each part ends with ix00 / ix01 chunks
    // that hold the chunk positions; the super indexes in hdrl point at those chunks. No idx1.
    let super_index = |chunk_id: &[u8; 4], entries: &[(u64, u32)]| {
        let mut b = Vec::new();
        b.extend_from_slice(&[4, 0, 0, 0]); // wLongsPerEntry = 4, subtype 0, type 0 (super)
        b.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        b.extend_from_slice(chunk_id);
        b.extend_from_slice(&[0; 12]);
        for (off, size) in entries {
            b.extend_from_slice(&off.to_le_bytes());
            b.extend_from_slice(&size.to_le_bytes());
            b.extend_from_slice(&5u32.to_le_bytes());
        }
        b
    };
    // The header's size does not depend on the entries, so build it once with zeros, measure,
    // then fill in the real positions.
    let header = |pos: &[[(u64, u32); 2]; 2]| -> Vec<u8> {
        let [pv, pa] = [0, 1].map(|s| vec![pos[0][s], pos[1][s]]);
        hdrl([Some(super_index(b"00dc", &pv)), Some(super_index(b"01wb", &pa))])
    };
    let h0 = header(&[[(0u64, 0u32); 2]; 2]);
    // layout: RIFF header 12, hdrl, movi LIST header 12, chunks, ix chunks
    let part_chunks = |range: std::ops::Range<usize>| -> (Vec<u8>, Vec<(u64, u32)>, Vec<(u64, u32)>, Vec<(u32, u32)>) {
        // returns the chunk bytes and, per chunk, (offset within the chunk bytes, size) for video and sound
        let (mut bytes, mut v, mut a) = (Vec::new(), Vec::new(), Vec::new());
        for n in range {
            let c = chunk(b"00dc", &frame(n));
            v.push((bytes.len() as u64 + 8, frame(n).len() as u32));
            bytes.extend(c);
            let c = chunk(b"01wb", &sound(n));
            a.push((bytes.len() as u64 + 8, 320u32));
            bytes.extend(c);
        }
        (bytes, v, a, Vec::new())
    };
    let ix = |chunk_id: &[u8; 4], base: u64, items: &[(u64, u32)], keys: &[bool]| {
        let mut b = Vec::new();
        b.extend_from_slice(&[2, 0, 0, 1]); // wLongsPerEntry = 2, subtype 0, type 1 (chunks)
        b.extend_from_slice(&(items.len() as u32).to_le_bytes());
        b.extend_from_slice(chunk_id);
        b.extend_from_slice(&base.to_le_bytes());
        b.extend_from_slice(&[0; 4]);
        for (k, (off, size)) in items.iter().enumerate() {
            b.extend_from_slice(&(*off as u32).to_le_bytes());
            b.extend_from_slice(&(if keys[k] { *size } else { size | 0x8000_0000 }).to_le_bytes());
        }
        b
    };
    // part 1 positions
    let (c1, v1, a1, _) = part_chunks(0..5);
    let movi1_start = 12 + h0.len() + 12; // first chunk header
    let base1 = movi1_start as u64; // entries are offsets from the base to the chunk data
    let (v1_rel, a1_rel): (Vec<_>, Vec<_>) = (v1.iter().map(|&(o, s)| (o, s)).collect(), a1.iter().map(|&(o, s)| (o, s)).collect());
    let keys1 = [true, false, false, false, false];
    let ix00_1 = chunk(b"ix00", &ix(b"00dc", base1, &v1_rel, &keys1));
    let ix01_1 = chunk(b"ix01", &ix(b"01wb", base1, &a1_rel, &[true; 5]));
    let movi1_len = 4 + c1.len() + ix00_1.len() + ix01_1.len();
    let part1_end = 12 + h0.len() + 8 + movi1_len; // file position after part 1 (all even)
    // part 2 (AVIX): RIFF header 12, movi LIST header 12, chunks
    let (c2, v2, a2, _) = part_chunks(5..10);
    let base2 = (part1_end + 12 + 12) as u64;
    let keys2 = [true, false, false, false, false];
    let ix00_2 = chunk(b"ix00", &ix(b"00dc", base2, &v2, &keys2));
    let ix01_2 = chunk(b"ix01", &ix(b"01wb", base2, &a2, &[true; 5]));
    // positions of the ix chunks (chunk header positions)
    let p_ix00_1 = (movi1_start + c1.len()) as u64;
    let p_ix01_1 = p_ix00_1 + ix00_1.len() as u64;
    let p_ix00_2 = base2 + c2.len() as u64;
    let p_ix01_2 = p_ix00_2 + ix00_2.len() as u64;
    let pos = [[(p_ix00_1, ix00_1.len() as u32), (p_ix01_1, ix01_1.len() as u32)], [(p_ix00_2, ix00_2.len() as u32), (p_ix01_2, ix01_2.len() as u32)]];
    let h = header(&pos);
    assert_eq!(h.len(), h0.len());
    let movi1 = list(b"movi", &[&c1[..], &ix00_1, &ix01_1].concat());
    let movi2 = list(b"movi", &[&c2[..], &ix00_2, &ix01_2].concat());
    let mut file = riff(b"AVI ", &[h, movi1]);
    assert_eq!(file.len(), part1_end);
    file.extend(riff(b"AVIX", &[movi2]));
    check_synthetic("opendml", &file, 10, &[0, 5]);
}

/// A real OpenDML file over 1 GB (several `RIFF AVIX` parts): run with
/// `RUSTDECODER_BIG_AVI=<file> cargo test --release --test avi -- --ignored big`. Made with
/// `ffmpeg -f lavfi -i testsrc2=size=640x360:rate=30 -f lavfi -i sine -t 110 -c:v rawvideo -c:a pcm_s16le -f avi big.avi`.
#[test]
#[ignore]
fn big_opendml() {
    let Some(clip) = std::env::var_os("RUSTDECODER_BIG_AVI") else { return };
    let clip = std::path::PathBuf::from(clip);
    let movie = open(&clip);
    let v = &movie.tracks[0];
    let a = &movie.tracks[1];
    let nv = check_track(&clip, v, "v:0", true);
    let na = check_track(&clip, a, "a:0", true);
    let len = std::fs::metadata(&clip).unwrap().len();
    assert!(v.samples.last().unwrap().offset > 1 << 30, "the last frame is past 1 GB");
    eprintln!("{}: {:.2} GB, {nv} video + {na} audio packets match ffprobe, last frame at byte {}", clip.display(), len as f64 / 1e9, v.samples.last().unwrap().offset);
}

#[cfg(windows)]
mod decode {
    use super::*;

    fn decode(name: &str, video: &[&str], audio: &[&str]) {
        if !have_ffmpeg() {
            return;
        }
        let clip = make_av(name, video, audio, AVI);
        let Some((n, first)) = ffcompare::compare_decode(&clip, 100_000) else { return };
        eprintln!("{name}: {n} frames decoded on the GPU and compared with ffmpeg: {first:?}");
        assert_eq!(n, 90, "{name}: frame count");
        assert_eq!(first, None, "{name}");
    }

    #[test]
    fn h264_pcm() {
        decode("a_h264_pcm.avi", H264, &["-c:a", "pcm_s16le"]);
    }

    #[test]
    fn h264_mp3_with_b_frames() {
        decode("a_h264_mp3_b.avi", H264_B, &["-c:a", "libmp3lame", "-b:a", "128k"]);
    }
}
