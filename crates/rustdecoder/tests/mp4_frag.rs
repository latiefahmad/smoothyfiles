//! Fragmented MP4 (`moof` + `mdat`) against ffprobe: every packet's size, presentation and
//! decode time and keyframe flag has to match, for the ways ffmpeg's muxer fragments a file
//! (and for a copy that was cut in the middle of its last `mdat`).

mod common;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::*;
use rustdecoder::mp4::{Kind, Track};

#[derive(Debug, PartialEq, Eq, Clone)]
struct Pkt {
    pts: i64,
    dts: i64,
    size: u32,
    key: bool,
}

/// Packets as ffprobe lists them, with their file position.
fn ffprobe_pos(clip: &Path, stream: &str) -> Vec<(Pkt, u64)> {
    let o = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", stream, "-show_entries", "packet=pts,dts,size,flags,pos", "-of", "compact"])
        .arg(clip)
        .output()
        .expect("ffprobe");
    assert!(o.status.success(), "ffprobe failed");
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .filter(|l| l.starts_with("packet"))
        .map(|l| {
            let get = |k: &str| l.split('|').find_map(|f| f.strip_prefix(&format!("{k}="))).unwrap_or("");
            (
                Pkt { pts: get("pts").parse().unwrap(), dts: get("dts").parse().unwrap(), size: get("size").parse().unwrap(), key: get("flags").starts_with('K') },
                get("pos").parse().unwrap(),
            )
        })
        .collect()
}

fn ffprobe(clip: &Path, stream: &str) -> Vec<Pkt> {
    ffprobe_pos(clip, stream).into_iter().map(|p| p.0).collect()
}

const H264: &[&str] = FRAG_H264;
const HEVC: &[&str] = FRAG_HEVC;
use common::make_frag as make;

fn compare_track(clip: &Path, t: &Track, stream: &str) -> usize {
    let mut want = ffprobe_pos(clip, stream);
    let got: Vec<Pkt> = t.samples.iter().map(|s| Pkt { pts: s.pts, dts: s.dts, size: s.size, key: s.key }).collect();
    // ffprobe hands out the partial packet a cut file ends with (its size is what is left);
    // we keep whole samples only
    let len = std::fs::metadata(clip).unwrap().len();
    if want.len() == got.len() + 1 && want.last().is_some_and(|(p, pos)| pos + p.size as u64 == len) {
        want.pop();
    }
    assert_eq!(got.len(), want.len(), "{}: {stream} packet count", clip.display());
    // Negative composition offsets (CMAF, `negative_cts_offsets`): the spec has the presentation
    // time as decode time + offset. ffmpeg moves the times by the most negative offset so no
    // presentation time is below its decode time: every pts up (fragments) or every dts down
    // (moov tables). Offsets, sizes, keyframes and the spacing are the same; each of pts and dts
    // may differ by one constant, which is only allowed for those clips.
    let pts_shift = want.first().map_or(0, |w| w.0.pts - got[0].pts);
    let dts_shift = want.first().map_or(0, |w| w.0.dts - got[0].dts);
    for (i, (g, w)) in got.iter().zip(&want).enumerate() {
        assert_eq!(t.samples[i].offset, w.1, "{}: {stream} packet {i} position", clip.display());
        let g = Pkt { pts: g.pts + pts_shift, dts: g.dts + dts_shift, ..g.clone() };
        assert_eq!(g, w.0, "{}: {stream} packet {i} (pts shift {pts_shift}, dts shift {dts_shift})", clip.display());
    }
    if pts_shift != 0 || dts_shift != 0 {
        eprintln!("{}: {stream}: ffprobe moves pts by {pts_shift}, dts by {dts_shift} (negative composition offsets)", clip.display());
        let n = clip.to_string_lossy();
        assert!(n.contains("cmaf") || n.contains("negcts") || n.contains("mixedneg"));
    }
    got.len()
}

/// Compares both tracks with ffprobe; returns (video packets, audio packets, video keys).
fn check(clip: &Path) -> (usize, usize, usize) {
    let movie = rustdecoder::container::read(&mut File::open(clip).unwrap()).unwrap();
    let v = movie.tracks.iter().find(|t| t.kind == Kind::Video).expect("video track");
    let a = movie.tracks.iter().find(|t| t.kind == Kind::Audio).expect("audio track");
    assert_eq!((v.width, v.height), (640, 360));
    let nv = compare_track(clip, v, "v:0");
    let na = compare_track(clip, a, "a:0");
    // every sample lies inside the file and reads back whole
    let len = std::fs::metadata(clip).unwrap().len();
    for t in [v, a] {
        assert!(t.samples.iter().all(|s| s.offset + s.size as u64 <= len));
    }
    (nv, na, v.samples.iter().filter(|s| s.key).count())
}

fn run(name: &str, video: &[&str], mux: &[&str], want_video: usize) -> Option<PathBuf> {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg not found");
        return None;
    }
    let clip = make(name, video, mux);
    let (nv, na, keys) = check(&clip);
    eprintln!("{name}: {nv} video + {na} audio packets match ffprobe ({keys} keyframes)");
    assert_eq!(nv, want_video);
    Some(clip)
}

macro_rules! variants {
    ($($fn:ident, $name:literal, $mux:expr;)*) => {
        mod h264 { use super::*; $(#[test] fn $fn() { run(concat!("frag_h264_", $name, ".mp4"), H264, $mux, 90); })* }
        mod hevc { use super::*; $(#[test] fn $fn() { run(concat!("frag_hevc_", $name, ".mp4"), HEVC, $mux, 90); })* }
    };
}

variants! {
    frag_keyframe_empty_moov, "kf", &["-movflags", "frag_keyframe+empty_moov"];
    default_base_moof, "dbm", &["-movflags", "frag_keyframe+empty_moov+default_base_moof"];
    frag_duration, "dur", &["-movflags", "empty_moov", "-frag_duration", "500000"];
    dash, "dash", &["-movflags", "+dash"];
    separate_moof, "sep", &["-movflags", "frag_keyframe+empty_moov+separate_moof"];
    cmaf, "cmaf", &["-movflags", "+cmaf"];
    negative_cts, "negcts", &["-movflags", "frag_keyframe+empty_moov+negative_cts_offsets", "-avoid_negative_ts", "disabled"];
    mixed_moov_and_fragments, "mixed", &["-movflags", "frag_keyframe"];
    one_fragment_per_frame, "perframe", &["-movflags", "empty_moov+default_base_moof", "-frag_duration", "1"];
}

/// Top-level boxes as (start, size, type).
fn top_boxes(b: &[u8]) -> Vec<(usize, usize, [u8; 4])> {
    let mut out = Vec::new();
    let mut p = 0;
    while p + 8 <= b.len() {
        let size = u32::from_be_bytes(b[p..p + 4].try_into().unwrap()) as usize;
        out.push((p, size, b[p + 4..p + 8].try_into().unwrap()));
        p += size.max(8);
    }
    out
}

#[test]
fn cut_in_the_middle_of_the_last_mdat() {
    let Some(clip) = run("frag_h264_kf.mp4", H264, &["-movflags", "frag_keyframe+empty_moov"], 90) else { return };
    let b = std::fs::read(&clip).unwrap();
    let boxes = top_boxes(&b);
    let (start, size, _) = *boxes.iter().rev().find(|x| &x.2 == b"mdat").unwrap();
    let cut = temp_dir().join("frag_h264_kf_cut.mp4");
    std::fs::write(&cut, &b[..start + 8 + (size - 8) / 2]).unwrap();
    let (nv, na, _) = check(&cut);
    eprintln!("cut in the last mdat: {nv} video + {na} audio packets match ffprobe");
    assert!(nv < 90 && nv > 60);
    // and cut inside the last moof: that fragment is dropped
    let (mstart, msize, _) = *boxes.iter().rev().find(|x| &x.2 == b"moof").unwrap();
    let cut2 = temp_dir().join("frag_h264_kf_cut_moof.mp4");
    std::fs::write(&cut2, &b[..mstart + msize / 2]).unwrap();
    let movie = rustdecoder::container::read(&mut File::open(&cut2).unwrap()).unwrap();
    let v = movie.video().unwrap();
    eprintln!("cut in the last moof: {} video packets, ffprobe says {}", v.samples.len(), ffprobe(&cut2, "v:0").len());
    assert_eq!(v.samples.len(), ffprobe(&cut2, "v:0").len());
}

/// Negative composition offsets in the `moov` tables followed by fragments: ffprobe shifts the
/// decode times of the `moov` part down and leaves the fragments' alone (its dts jumps at the
/// boundary), so the times cannot be compared exactly. Counts, sizes, positions and keyframes
/// match it; ours run on without a jump, and every picture shows after it is decoded.
#[test]
fn mixed_with_negative_offsets_is_continuous() {
    if !have_ffmpeg() {
        return;
    }
    let clip = make("frag_h264_mixedneg.mp4", H264, &["-movflags", "frag_keyframe+negative_cts_offsets"]);
    let movie = rustdecoder::container::read(&mut File::open(&clip).unwrap()).unwrap();
    let v = movie.video().unwrap();
    let want = ffprobe_pos(&clip, "v:0");
    assert_eq!(v.samples.len(), want.len());
    for (s, (w, pos)) in v.samples.iter().zip(&want) {
        assert_eq!((s.size, s.offset, s.key), (w.size, *pos, w.key));
    }
    assert!(v.samples.windows(2).all(|p| p[1].dts - p[0].dts == 512), "decode times run on in steps of one frame");
    assert!(v.samples.iter().all(|s| s.pts >= v.samples[0].dts));
    let mut pts: Vec<i64> = v.samples.iter().map(|s| s.pts).collect();
    pts.sort();
    assert!(pts.windows(2).all(|p| p[1] - p[0] == 512), "every presentation time once, one frame apart");
}

#[test]
fn cut_mixed_file() {
    let Some(clip) = run("frag_h264_mixed.mp4", H264, &["-movflags", "frag_keyframe"], 90) else { return };
    let b = std::fs::read(&clip).unwrap();
    let boxes = top_boxes(&b);
    let (start, size, _) = *boxes.iter().rev().find(|x| &x.2 == b"mdat").unwrap();
    let cut = temp_dir().join("frag_h264_mixed_cut.mp4");
    std::fs::write(&cut, &b[..start + 8 + (size - 8) / 2]).unwrap();
    let (nv, na, _) = check(&cut);
    eprintln!("cut mixed file: {nv} video + {na} audio packets match ffprobe");
}

// ------------------------------------------------------------------ hand-built files

fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut v = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
    v.extend_from_slice(kind);
    v.extend_from_slice(payload);
    v
}

fn be(vals: &[u32]) -> Vec<u8> {
    vals.iter().flat_map(|v| v.to_be_bytes()).collect()
}

/// A movie with one video track (id 1, timescale 1000), no samples in `moov`, `trex` defaults of
/// duration 40, size 0, flags 0x0101_0000 (non-sync, depends on others).
fn bare_moov() -> Vec<u8> {
    let mut mvhd = vec![0u8; 100];
    mvhd[12..16].copy_from_slice(&1000u32.to_be_bytes());
    let mut tkhd = vec![0u8; 84];
    tkhd[12..16].copy_from_slice(&1u32.to_be_bytes());
    let mut mdhd = vec![0u8; 24];
    mdhd[12..16].copy_from_slice(&1000u32.to_be_bytes());
    let mut hdlr = vec![0u8; 8];
    hdlr.extend_from_slice(b"vide");
    hdlr.extend_from_slice(&[0u8; 13]);
    let stbl = [bx(b"stsd", &be(&[0, 0])), bx(b"stsz", &be(&[0, 0, 0])), bx(b"stts", &be(&[0, 0])), bx(b"stsc", &be(&[0, 0])), bx(b"stco", &be(&[0, 0]))].concat();
    let minf = bx(b"minf", &bx(b"stbl", &stbl));
    let mdia = bx(b"mdia", &[bx(b"mdhd", &mdhd), bx(b"hdlr", &hdlr), minf].concat());
    let trak = bx(b"trak", &[bx(b"tkhd", &tkhd), mdia].concat());
    let mvex = bx(b"mvex", &bx(b"trex", &be(&[0, 1, 1, 40, 0, 0x0101_0000])));
    bx(b"moov", &[bx(b"mvhd", &mvhd), trak, mvex].concat())
}

/// Fragments written the way ffmpeg never does: a base-data-offset, defaults from `tfhd` and
/// `trex`, no `tfdt` in the second fragment, two `trun`s in one `traf` (the second without a data
/// offset, so it follows the first), first-sample-flags, per-sample flags, signed composition
/// offsets (version 1), and an unknown track that has to be skipped.
#[test]
fn hand_built_fragments() {
    let mut file = bx(b"ftyp", b"iso5\0\0\0\0iso5");
    file.extend(bare_moov());
    let data_start = file.len() as u64;
    // two moofs of known size: build with a placeholder offset first to learn the size
    let build = |moof_data_offset: i32, tfdt: bool| -> Vec<u8> {
        // tfhd: base-data-offset 0x01, default duration 0x08, default size 0x10
        let mut tfhd = be(&[0x0000_0019, 1]);
        tfhd.extend_from_slice(&0u64.to_be_bytes()); // base data offset: start of the file
        tfhd.extend(be(&[50, 100])); // default duration 50, size 100
        // trun A: data offset, first-sample-flags (sync), 3 samples with cts v1 (signed)
        let mut run_a = be(&[0x0100_0005 | 0x800 << 0, 3]);
        run_a[0..4].copy_from_slice(&(0x0100_0000u32 | 0x0000_0805).to_be_bytes());
        run_a.extend(be(&[moof_data_offset as u32, 0x0200_0000]));
        run_a.extend(be(&[(-100i32) as u32, 100, 0]));
        // trun B: sizes and flags per sample (0x200 | 0x400), no data offset: follows A
        let mut run_b = be(&[0x0000_0600, 2]);
        run_b.extend(be(&[30, 0x0200_0000, 70, 0x0001_0000]));
        let mut traf = bx(b"tfhd", &tfhd);
        if tfdt {
            traf.extend(bx(b"tfdt", &[be(&[0x0100_0000]), 1000u64.to_be_bytes().to_vec()].concat()));
        }
        traf.extend(bx(b"trun", &run_a));
        traf.extend(bx(b"trun", &run_b));
        let other = bx(b"traf", &bx(b"tfhd", &be(&[0, 9])));
        bx(b"moof", &[bx(b"mfhd", &be(&[0, 1])), bx(b"traf", &traf), other].concat())
    };
    let size1 = build(0, true).len() as u64;
    let first = build((data_start + size1 + 8) as i32, true);
    assert_eq!(first.len() as u64, size1);
    file.extend(&first);
    let payload1 = 100 * 3 + 30 + 70;
    file.extend(bx(b"mdat", &vec![0xAAu8; payload1]));
    let second_start = file.len() as u64;
    // the second fragment: no tfdt (continues), data right after its own mdat header
    let size2 = build(0, false).len() as u64;
    let second = build((second_start + size2 + 8) as i32, false);
    file.extend(&second);
    file.extend(bx(b"mdat", &vec![0xBBu8; payload1]));

    let movie = rustdecoder::mp4::read(&mut std::io::Cursor::new(file.clone())).unwrap();
    let t = &movie.tracks[0];
    assert_eq!(t.samples.len(), 10);
    let s = &t.samples;
    // first fragment: tfdt 1000, durations 50 (default), cts -100 / 100 / 0 from the run
    let at = data_start + size1 + 8;
    assert_eq!((s[0].dts, s[0].pts, s[0].size, s[0].offset, s[0].key), (1000, 900, 100, at, true));
    assert_eq!((s[1].dts, s[1].pts, s[1].size, s[1].offset, s[1].key), (1050, 1150, 100, at + 100, false));
    assert_eq!((s[2].dts, s[2].pts, s[2].offset, s[2].key), (1100, 1100, at + 200, false));
    // run B follows run A's data; its flags are per sample (the second is non-sync)
    assert_eq!((s[3].dts, s[3].size, s[3].offset, s[3].key), (1150, 30, at + 300, true));
    assert_eq!((s[4].dts, s[4].size, s[4].offset, s[4].key), (1200, 70, at + 330, false));
    // second fragment: no tfdt, the times go on from the last sample (duration 50)
    assert_eq!(s[5].dts, 1250);
    assert_eq!(s[9].dts, 1450);
    assert_eq!(s[5].offset, second_start + size2 + 8);
    assert_eq!(movie.duration_secs(), (1450.0 + 50.0) / 1000.0);

    // the second moof cut in half is dropped, the first fragment stays
    let cut = &file[..second_start as usize + 20];
    let m = rustdecoder::mp4::read(&mut std::io::Cursor::new(cut.to_vec())).unwrap();
    assert_eq!(m.tracks[0].samples.len(), 5);
    // the last mdat cut after 150 bytes: the samples fully inside stay (sample 5 and 6 + ...)
    let cut = &file[..(second_start + size2 + 8) as usize + 150];
    let m = rustdecoder::mp4::read(&mut std::io::Cursor::new(cut.to_vec())).unwrap();
    assert_eq!(m.tracks[0].samples.len(), 6);
}

#[test]
fn plain_files_are_unchanged() {
    // a file without fragments reads exactly as before (tests/mp4.rs covers its numbers)
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/h264_aac.mp4");
    let m = rustdecoder::mp4::read(&mut File::open(p).unwrap()).unwrap();
    assert_eq!(m.video().unwrap().samples.len(), 25);
}
