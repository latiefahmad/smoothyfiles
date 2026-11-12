//! The MP4 reader against ffprobe's view of the same files (tests/data, made with ffmpeg).

use std::fs::File;
use std::path::Path;

use rustdecoder::mp4::{self, Codec, Kind};

fn open(name: &str) -> mp4::Movie {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(name);
    mp4::read(&mut File::open(p).unwrap()).unwrap()
}

/// (pts, dts, size, offset, key) per video packet, from
/// `ffprobe -select_streams v:0 -show_entries packet=pts,dts,size,pos,flags`.
const H264: &[(i64, i64, u32, u64, bool)] = &[
    (0, -1024, 1593, 2155, true),
    (1536, -512, 59, 3748, false),
    (512, 0, 13, 3949, false),
    (1024, 512, 13, 4178, false),
    (3072, 1024, 36, 4313, false),
    (2048, 1536, 13, 4490, false),
    (2560, 2048, 14, 4665, false),
    (4608, 2560, 46, 4841, false),
    (3584, 3072, 13, 5050, false),
    (4096, 3584, 13, 5238, false),
    (5120, 4096, 970, 5344, true),
    (6656, 4608, 25, 6486, false),
    (5632, 5120, 12, 6677, false),
    (6144, 5632, 13, 6859, false),
    (8192, 6144, 46, 7042, false),
    (7168, 6656, 14, 7253, false),
    (7680, 7168, 18, 7451, false),
    (9728, 7680, 69, 7639, false),
    (8704, 8192, 15, 7791, false),
    (9216, 8704, 16, 7996, false),
    (10240, 9216, 949, 8176, true),
    (11776, 9728, 53, 9281, false),
    (10752, 10240, 14, 9507, false),
    (11264, 10752, 15, 9704, false),
    (12288, 11264, 18, 9896, false),
];

#[test]
fn h264_with_b_frames_and_aac() {
    let m = open("h264_aac.mp4");
    let v = m.video().unwrap();
    assert_eq!(v.kind, Kind::Video);
    assert_eq!(&v.fourcc, b"avc1");
    assert!(matches!(&v.codec, Codec::H264 { config } if config[0] == 1));
    assert_eq!((v.width, v.height, v.timescale, v.rotation), (64, 48, 12800, 0));
    assert_eq!(v.samples.len(), H264.len());
    for (s, &(pts, dts, size, offset, key)) in v.samples.iter().zip(H264) {
        assert_eq!((s.pts, s.dts, s.size, s.offset, s.key), (pts, dts, size, offset, key));
    }
    assert!((v.frame_rate() - 25.0).abs() < 1e-9);

    let a = m.audio().unwrap();
    assert_eq!(&a.fourcc, b"mp4a");
    // AAC LC, 48 kHz (index 3), mono
    assert!(matches!(&a.codec, Codec::Aac { config } if config[..2] == [0x11, 0x88]));
    assert_eq!((a.channels, a.sample_rate, a.timescale), (1, 48000, 48000));
    assert_eq!(a.samples.len(), 48);
    assert_eq!(a.samples.iter().map(|s| s.size as u64).sum::<u64>(), 4126);
    assert_eq!((a.samples[0].pts, a.samples[0].size, a.samples[0].offset), (-1024, 142, 3807));
    assert_eq!((a.samples[1].pts, a.samples[1].size, a.samples[1].offset), (0, 159, 3962));
    assert!((m.duration_secs() - 1.0).abs() < 0.05);
}

#[test]
fn hevc_10_bit_rotated_mov() {
    let m = open("hevc10_rot.mov");
    let v = m.video().unwrap();
    assert_eq!(&v.fourcc, b"hvc1");
    assert!(matches!(&v.codec, Codec::Hevc { config } if config[0] == 1));
    assert_eq!((v.width, v.height, v.timescale), (64, 48, 30000));
    // ffmpeg's -display_rotation 90 turns the picture counter-clockwise: 270 clockwise
    assert_eq!(v.rotation, 270);
    let pts: Vec<i64> = v.samples.iter().map(|s| s.pts).collect();
    assert_eq!(pts, [0, 3003, 2002, 1001, 5005, 4004, 8008, 7007, 6006, 10010, 9009, 14014, 12012, 11011, 13013]);
    assert_eq!(v.samples[0].dts, -2002);
    assert_eq!((v.samples[0].size, v.samples[0].offset), (719, 36));
    assert_eq!(v.samples.iter().filter(|s| s.key).count(), 2);
    assert!(v.samples[6].key);
    assert!((v.frame_rate() - 30000.0 / 1001.0).abs() < 1e-6);
}

#[test]
fn samples_read_back() {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/hevc10.mp4");
    let mut f = File::open(&p).unwrap();
    let m = mp4::read(&mut f).unwrap();
    let v = m.video().unwrap();
    let mut buf = Vec::new();
    v.read_sample(&mut f, 0, &mut buf).unwrap();
    assert_eq!(buf.len(), v.samples[0].size as usize);
    // length-prefixed NAL units: the first length fits in the sample
    let n = u32::from_be_bytes(buf[..4].try_into().unwrap()) as usize;
    assert!(n + 4 <= buf.len());
}

#[test]
fn not_an_mp4() {
    let mut c = std::io::Cursor::new(b"this is plain text, not a movie".to_vec());
    assert!(mp4::read(&mut c).is_err());
    let mut c = std::io::Cursor::new(Vec::new());
    assert!(mp4::read(&mut c).is_err());
}

/// A real camera file, when one is named: `RUSTDECODER_SAMPLE=<file> cargo test -- --ignored`.
#[test]
#[ignore]
fn camera_file() {
    let Some(p) = std::env::var_os("RUSTDECODER_SAMPLE") else { return };
    let t = std::time::Instant::now();
    let m = mp4::read(&mut File::open(&p).unwrap()).unwrap();
    let el = t.elapsed();
    for t in &m.tracks {
        let codec = match &t.codec {
            Codec::H264 { .. } => "h264".to_string(),
            Codec::Hevc { .. } => "hevc".to_string(),
            Codec::Aac { .. } => "aac".to_string(),
            Codec::Pcm { bits, float, big_endian } => format!("pcm {bits}{} {}", if *float { "f" } else { "" }, if *big_endian { "be" } else { "le" }),
            Codec::Vp9 { .. } => "vp9".to_string(),
            Codec::Av1 { .. } => "av1".to_string(),
            Codec::Opus { .. } => "opus".to_string(),
            Codec::Vorbis { .. } => "vorbis".to_string(),
            Codec::Flac { .. } => "flac".to_string(),
            Codec::Mp3 => "mp3".to_string(),
            Codec::Ac3 { .. } => "ac3".to_string(),
            Codec::Eac3 { .. } => "eac3".to_string(),
            Codec::Other => "other".to_string(),
        };
        eprintln!(
            "track {} {:?} {} {codec} {}x{} rot {} {} ch {} Hz, {} samples ({} key), {:.3} fps",
            t.id,
            t.kind,
            String::from_utf8_lossy(&t.fourcc),
            t.width,
            t.height,
            t.rotation,
            t.channels,
            t.sample_rate,
            t.samples.len(),
            t.samples.iter().filter(|s| s.key).count(),
            t.frame_rate()
        );
    }
    for t in &m.tracks {
        eprintln!("track {} bytes {}", t.id, t.samples.iter().map(|s| s.size as u64).sum::<u64>());
    }
    eprintln!("read in {el:?}, {:.3} s", m.duration_secs());
}
