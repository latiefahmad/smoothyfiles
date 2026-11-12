//! The Matroska / WebM reader against ffprobe: every packet's count, size, presentation time and
//! keyframe flag has to match. Clips are made with ffmpeg into the temp folder; a hand-built
//! file covers the things ffmpeg's muxer never writes (lacing, block groups, unknown-size
//! elements in odd places).

mod common;

use std::fs::File;

use common::*;
use rustdecoder::mkv;
use rustdecoder::mp4::{Codec, Kind, Track};

fn compare_track(clip: &std::path::Path, t: &Track, stream: &str) {
    let want = ffprobe_packets(clip, stream);
    let got: Vec<Packet> = t.samples.iter().map(|s| Packet { pts: s.pts, size: s.size, key: s.key }).collect();
    assert_eq!(got.len(), want.len(), "{}: packet count", clip.display());
    for (i, (g, w)) in got.iter().zip(&want).enumerate() {
        assert_eq!(g, w, "{}: packet {i}", clip.display());
    }
}

/// H.264 / HEVC samples are length prefixed: the NAL lengths have to add up to the sample.
fn check_nal_lengths(t: &Track, clip: &std::path::Path, len_size: usize) {
    let mut f = File::open(clip).unwrap();
    let mut buf = Vec::new();
    for i in (0..t.samples.len()).step_by(7) {
        t.read_sample(&mut f, i, &mut buf).unwrap();
        let mut p = 0usize;
        while p < buf.len() {
            assert!(p + len_size <= buf.len(), "sample {i}: cut length");
            let n = buf[p..p + len_size].iter().fold(0usize, |a, &b| a << 8 | b as usize);
            p += len_size + n;
        }
        assert_eq!(p, buf.len(), "sample {i}: NAL units do not fill the sample");
    }
}

fn check_clip(name: &str, video: &[&str], audio: &[&str], mux: &[&str], a_codec: fn(&Codec) -> bool) {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg not found");
        return;
    }
    let clip = make_mkv(name, video, audio, mux);
    let movie = rustdecoder::container::read(&mut File::open(&clip).unwrap()).unwrap();
    let v = movie.tracks.iter().find(|t| t.kind == Kind::Video).expect("video track");
    let a = movie.tracks.iter().find(|t| t.kind == Kind::Audio).expect("audio track");
    assert!(a_codec(&a.codec), "{name}: audio codec {:?}", a.codec);
    assert_eq!((v.width, v.height), (640, 360));
    assert_eq!((a.sample_rate, a.channels), (48000, 1));
    compare_track(&clip, v, "v:0");
    compare_track(&clip, a, "a:0");
    match &v.codec {
        Codec::H264 { config } => check_nal_lengths(v, &clip, (config[4] & 3) as usize + 1),
        Codec::Hevc { config } => check_nal_lengths(v, &clip, (config[21] & 3) as usize + 1),
        Codec::Vp9 { .. } => {
            let mut f = File::open(&clip).unwrap();
            let mut buf = Vec::new();
            for i in 0..v.samples.len() {
                v.read_sample(&mut f, i, &mut buf).unwrap();
                assert_eq!(buf[0] >> 6, 2, "VP9 frame marker, sample {i}");
            }
        }
        c => panic!("{c:?}"),
    }
    assert!((movie.duration_secs() - 3.0).abs() < 0.1, "duration {}", movie.duration_secs());
    let keys = v.samples.iter().filter(|s| s.key).count();
    eprintln!("{name}: {} video + {} audio packets match ffprobe ({keys} sync samples, timescale {})", v.samples.len(), a.samples.len(), v.timescale);
}

const H264: &[&str] = &["-c:v", "libx264", "-pix_fmt", "yuv420p", "-preset", "veryfast", "-x264-params", "log-level=error:keyint=30:bframes=3"];
const HEVC: &[&str] = &["-c:v", "libx265", "-pix_fmt", "yuv420p", "-preset", "ultrafast", "-x265-params", "log-level=error:keyint=30"];
const VP9: &[&str] = &["-c:v", "libvpx-vp9", "-pix_fmt", "yuv420p", "-deadline", "good", "-cpu-used", "5", "-lag-in-frames", "25", "-auto-alt-ref", "1", "-g", "45", "-b:v", "1M"];

fn is_aac(c: &Codec) -> bool {
    matches!(c, Codec::Aac { .. })
}
fn is_pcm(c: &Codec) -> bool {
    matches!(c, Codec::Pcm { bits: 16, float: false, big_endian: false })
}
fn is_opus(c: &Codec) -> bool {
    matches!(c, Codec::Opus { .. })
}
fn is_vorbis(c: &Codec) -> bool {
    matches!(c, Codec::Vorbis { .. })
}

#[test]
fn h264_aac_in_mkv() {
    check_clip("mkv_h264_aac.mkv", H264, &["-c:a", "aac", "-b:a", "96k"], &[], is_aac);
}

#[test]
fn hevc_pcm_in_mkv() {
    check_clip("mkv_hevc_pcm.mkv", HEVC, &["-c:a", "pcm_s16le"], &[], is_pcm);
}

#[test]
fn vp9_opus_in_webm() {
    check_clip("webm_vp9_opus.webm", VP9, &["-c:a", "libopus", "-b:a", "64k"], &[], is_opus);
}

#[test]
fn vp9_vorbis_in_webm() {
    check_clip("webm_vp9_vorbis.webm", VP9, &["-c:a", "libvorbis", "-b:a", "64k"], &[], is_vorbis);
}

#[test]
fn small_clusters_and_reserved_index() {
    check_clip("mkv_h264_clusters.mkv", H264, &["-c:a", "aac"], &["-cluster_size_limit", "30000", "-cluster_time_limit", "300", "-reserve_index_space", "4000"], is_aac);
}

#[test]
fn live_stream_unknown_sizes() {
    check_clip("mkv_h264_live.mkv", H264, &["-c:a", "aac"], &["-live", "1"], is_aac);
}

#[test]
fn live_webm_unknown_sizes() {
    check_clip("webm_vp9_live.webm", VP9, &["-c:a", "libopus"], &["-live", "1", "-cluster_time_limit", "500"], is_opus);
}

#[test]
fn colour_and_default_flag() {
    if !have_ffmpeg() {
        return;
    }
    let clip = make_mkv(
        "mkv_h264_bt2020b.mkv",
        &["-c:v", "libx264", "-pix_fmt", "yuv420p", "-preset", "veryfast", "-vf", "setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc:range=tv"],
        &["-c:a", "aac"],
        &[],
    );
    let m = mkv::read(&mut File::open(clip).unwrap()).unwrap();
    let v = m.video().unwrap();
    let c = v.colour.expect("Colour element");
    assert_eq!((c.matrix, c.transfer, c.primaries, c.full_range), (9, 16, 9, Some(false)));
    eprintln!("FlagDefault of the video track: {}", v.default);
}

// ------------------------------------------------------------------ hand-built files

fn vint_size(n: u64) -> Vec<u8> {
    // always 8 bytes: simple and valid
    let mut v = vec![0x01];
    v.extend_from_slice(&n.to_be_bytes()[1..]);
    v
}

fn el(id: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut v = id.to_vec();
    v.extend(vint_size(payload.len() as u64));
    v.extend_from_slice(payload);
    v
}

fn el_unknown(id: &[u8]) -> Vec<u8> {
    let mut v = id.to_vec();
    v.extend_from_slice(&[0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
    v
}

fn uint_el(id: &[u8], n: u64) -> Vec<u8> {
    el(id, &n.to_be_bytes())
}

fn block(track: u8, rel: i16, flags: u8, body: &[u8]) -> Vec<u8> {
    let mut b = vec![0x80 | track];
    b.extend_from_slice(&rel.to_be_bytes());
    b.push(flags);
    b.extend_from_slice(body);
    b
}

/// A file with one 48 kHz mono PCM track (DefaultDuration 10 ms) and one video track, unknown
/// size Segment and Clusters, laced and grouped blocks. Returns it and the audio frames
/// expected as (pts, size).
fn laced_file() -> (Vec<u8>, Vec<(i64, u32)>) {
    let mut f = el(&[0x1A, 0x45, 0xDF, 0xA3], &el(&[0x42, 0x82], b"matroska"));
    f.extend(el_unknown(&[0x18, 0x53, 0x80, 0x67]));
    f.extend(el(&[0x15, 0x49, 0xA9, 0x66], &[uint_el(&[0x2A, 0xD7, 0xB1], 1_000_000), el(&[0x44, 0x89], &10_000f32.to_be_bytes())].concat()));
    let audio = el(
        &[0xAE],
        &[
            uint_el(&[0xD7], 1),
            uint_el(&[0x83], 2),
            uint_el(&[0x23, 0xE3, 0x83], 10_000_000),
            el(&[0x86], b"A_PCM/INT/LIT"),
            el(&[0xE1], &[el(&[0xB5], &48000f32.to_be_bytes()), uint_el(&[0x9F], 1), uint_el(&[0x62, 0x64], 16)].concat()),
        ]
        .concat(),
    );
    let video = el(
        &[0xAE],
        &[uint_el(&[0xD7], 2), uint_el(&[0x83], 1), el(&[0x86], b"V_VP9"), el(&[0xE0], &[uint_el(&[0xB0], 64), uint_el(&[0xBA], 48)].concat())].concat(),
    );
    f.extend(el(&[0x16, 0x54, 0xAE, 0x6B], &[audio, video].concat()));

    let mut want = Vec::new();
    // cluster 1 at 1000 ms, unknown size
    f.extend(el_unknown(&[0x1F, 0x43, 0xB6, 0x75]));
    f.extend(uint_el(&[0xE7], 1000));
    // Xiph lacing, 3 frames of 4, 300 and 6 bytes
    let mut body = vec![2u8, 4, 255, 45];
    body.extend(vec![1u8; 4]);
    body.extend(vec![2u8; 300]);
    body.extend(vec![3u8; 6]);
    f.extend(el(&[0xA3], &block(1, 0, 0x80 | 0x02, &body)));
    want.extend([(1000, 4), (1010, 300), (1020, 6)]);
    // EBML lacing, 4 frames of 10, 12, 9 and the remaining 20 bytes: first size 10 (1 byte),
    // then +2 and -3 as signed 1-byte values (bias 63)
    let mut body = vec![3u8, 0x80 | 10, 0x80 | (63 + 2), 0x80 | (63 - 3)];
    body.extend(vec![4u8; 10 + 12 + 9 + 20]);
    f.extend(el(&[0xA3], &block(1, 40, 0x80 | 0x06, &body)));
    want.extend([(1040, 10), (1050, 12), (1060, 9), (1070, 20)]);
    // fixed-size lacing, 5 frames of 8 bytes
    let mut body = vec![4u8];
    body.extend(vec![5u8; 40]);
    f.extend(el(&[0xA3], &block(1, 80, 0x80 | 0x04, &body)));
    want.extend([(1080, 8), (1090, 8), (1100, 8), (1110, 8), (1120, 8)]);
    // a video key frame, a block group delta frame with a reference, a block group without one
    f.extend(el(&[0xA3], &block(2, 0, 0x80, &[0x82; 33])));
    f.extend(el(&[0xA0], &[el(&[0xA1], &block(2, 33, 0, &[0x82; 21])), uint_el(&[0x9B], 33), el(&[0xFB], &[0xDF])].concat()));
    f.extend(el(&[0xA0], &el(&[0xA1], &block(2, 66, 0, &[0x82; 17]))));

    // cluster 2 at 2000 ms, known size, after the unknown-size one
    let mut c = uint_el(&[0xE7], 2000);
    c.extend(el(&[0xA3], &block(1, 0, 0x80, &[7u8; 480])));
    want.push((2000, 480));
    c.extend(el(&[0xEC], &[0; 5])); // Void
    c.extend(el(&[0xA3], &block(1, 10, 0x80, &[7u8; 480])));
    want.push((2010, 480));
    f.extend(el(&[0x1F, 0x43, 0xB6, 0x75], &c));
    (f, want)
}

#[test]
fn lacing_block_groups_and_unknown_sizes() {
    let (bytes, want) = laced_file();
    let m = mkv::read(&mut std::io::Cursor::new(&bytes)).unwrap();
    assert_eq!(m.timescale, 1000);
    assert_eq!(m.duration, 10_000);
    let a = m.audio().unwrap();
    assert!(matches!(a.codec, Codec::Pcm { bits: 16, float: false, big_endian: false }));
    assert_eq!((a.sample_rate, a.channels), (48000, 1));
    let got: Vec<(i64, u32)> = a.samples.iter().map(|s| (s.pts, s.size)).collect();
    assert_eq!(got, want);
    // the bytes are where the table says
    for (i, s) in a.samples.iter().enumerate() {
        let body = &bytes[s.offset as usize..s.offset as usize + s.size as usize];
        let expect = match want[i].0 {
            1000..=1009 => 1,
            1010..=1019 => 2,
            1020..=1039 => 3,
            1040..=1079 => 4,
            1080..=1199 => 5,
            _ => 7,
        };
        assert!(body.iter().all(|&b| b == expect), "frame {i} points at the wrong bytes");
    }
    let v = m.video().unwrap();
    assert!(matches!(v.codec, Codec::Vp9 { .. }));
    assert_eq!((v.width, v.height), (64, 48));
    let got: Vec<(i64, u32, bool)> = v.samples.iter().map(|s| (s.pts, s.size, s.key)).collect();
    assert_eq!(got, vec![(1000, 33, true), (1033, 21, false), (1066, 17, true)]);
    if have_ffmpeg() {
        let p = temp_dir().join("handbuilt_lacing.mkv");
        std::fs::write(&p, &bytes).unwrap();
        let ff = ffprobe_packets(&p, "a:0");
        let ours: Vec<(i64, u32)> = ff.iter().map(|p| (p.pts, p.size)).collect();
        let _ = std::fs::remove_file(&p);
        assert_eq!(ours, want, "ffprobe reads the hand-built file the same way");
    }
}

#[test]
fn cut_file_keeps_what_was_read() {
    let (bytes, want) = laced_file();
    // cut in the middle of the last block
    let m = mkv::read(&mut std::io::Cursor::new(&bytes[..bytes.len() - 100])).unwrap();
    assert_eq!(m.audio().unwrap().samples.len(), want.len() - 1);
}

#[test]
fn not_matroska_is_an_error() {
    assert!(mkv::read(&mut std::io::Cursor::new(b"RIFF....WEBP".to_vec())).is_err());
    assert!(mkv::read(&mut std::io::Cursor::new(Vec::new())).is_err());
}
