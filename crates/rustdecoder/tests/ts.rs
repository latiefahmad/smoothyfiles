//! The transport stream reader against ffprobe: every packet's count, presentation and decode
//! time, keyframe flag, and its bytes (ffprobe's CRC32 over the packet) have to match.
//!
//! What is compared, exactly:
//! - times: ffprobe's raw 90 kHz PTS / DTS minus the earliest PTS of all streams (the reader's
//!   zero), converted to the track's timescale (audio: sample rate, one unit of rounding allowed);
//! - sync flag: equal;
//! - H.264 / HEVC, MPEG-2, AC-3, MPEG audio, LPCM: the stream bytes of the sample as stored
//!   (`Layout::read_raw`) have ffprobe's size and CRC32;
//! - AAC (ADTS) and Blu-ray LPCM: ffprobe keeps the 7-byte ADTS / 4-byte LPCM header, the reader
//!   strips it: `size + header == ffprobe size`; the bytes themselves are checked by decoding;
//! - H.264 / HEVC converted samples: the NAL lengths add up to the sample.

mod common;

use std::fs::File;
use std::path::Path;

use common::*;
use rustdecoder::mp4::{Codec, Kind, Storage, Track};

const H264: &[&str] = &["-c:v", "libx264", "-pix_fmt", "yuv420p", "-preset", "veryfast", "-x264-params", "log-level=error:keyint=30:bframes=3"];
const HEVC: &[&str] = &["-c:v", "libx265", "-pix_fmt", "yuv420p", "-preset", "ultrafast", "-x265-params", "log-level=error:keyint=30"];
const AAC: &[&str] = &["-c:a", "aac", "-b:a", "96k"];
const TS: &[&str] = &["-f", "mpegts"];
const M2TS: &[&str] = &["-f", "mpegts", "-mpegts_m2ts_mode", "1"];

#[derive(Clone, Copy, PartialEq)]
enum Bytes {
    /// Stream bytes equal ffprobe's packet.
    Same,
    /// ffprobe's packet has this many more bytes in front (ADTS header).
    Header(u32),
}

fn crc(b: &[u8]) -> u32 {
    // ffprobe's "CRC32" hash is the usual CRC-32 (zlib's)
    crc32fast::hash(b)
}

fn check_track(clip: &Path, t: &Track, stream: &str, base: i64, bytes: Bytes, tol: i64) -> usize {
    let want = ffprobe_full(clip, stream);
    assert_eq!(t.samples.len(), want.len(), "{}: {stream} packet count", clip.display());
    let Storage::Ts(layout) = &t.storage else { panic!("not a TS track") };
    let mut f = File::open(clip).unwrap();
    let mut raw = Vec::new();
    for (i, (s, w)) in t.samples.iter().zip(&want).enumerate() {
        let conv = |v: i64| ((v - base) as i128 * t.timescale as i128 + 45000).div_euclid(90000) as i64;
        if w.pts != i64::MIN {
            assert!((s.pts - conv(w.pts)).abs() <= tol, "{stream} packet {i}: pts {} want {}", s.pts, conv(w.pts));
        }
        if w.dts != i64::MIN {
            assert!((s.dts - conv(w.dts)).abs() <= tol, "{stream} packet {i}: dts {} want {}", s.dts, conv(w.dts));
        }
        assert_eq!(s.key, w.key, "{stream} packet {i}: key");
        match bytes {
            Bytes::Same => {
                assert_eq!(layout.raw_size(i), w.size, "{stream} packet {i}: size");
                layout.read_raw(&mut f, s, i, &mut raw).unwrap();
                assert_eq!(crc(&raw), w.crc, "{stream} packet {i}: CRC32");
            }
            Bytes::Header(h) => assert_eq!(s.size + h, w.size, "{stream} packet {i}: size"),
        }
    }
    want.len()
}

/// Every converted H.264 / HEVC sample is a run of 4-byte length prefixed NAL units.
fn check_nal_lengths(clip: &Path, t: &Track) {
    let mut f = File::open(clip).unwrap();
    let mut buf = Vec::new();
    for i in 0..t.samples.len() {
        t.read_sample(&mut f, i, &mut buf).unwrap();
        assert_eq!(buf.len() as u32, t.samples[i].size, "sample {i}: size");
        let mut p = 0usize;
        while p < buf.len() {
            assert!(p + 4 <= buf.len(), "sample {i}: cut length");
            let n = u32::from_be_bytes(buf[p..p + 4].try_into().unwrap()) as usize;
            p += 4 + n;
        }
        assert_eq!(p, buf.len(), "sample {i}: NAL units do not fill the sample");
    }
}

fn earliest(clip: &Path, streams: &[&str]) -> i64 {
    streams.iter().flat_map(|s| ffprobe_full(clip, s)).map(|p| p.pts).filter(|&p| p != i64::MIN).min().unwrap()
}

struct Expect {
    video: Option<(&'static str, (u32, u32))>,
    audio: Option<(fn(&Codec) -> bool, u32, u16, Bytes, i64)>,
}

fn check_clip(name: &str, video: &[&str], audio: &[&str], mux: &[&str], expect: Expect) {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg not found");
        return;
    }
    let clip = make_av(name, video, audio, mux);
    let movie = rustdecoder::container::read(&mut File::open(&clip).unwrap()).unwrap();
    let mut streams = Vec::new();
    if expect.video.is_some() {
        streams.push("v:0");
    }
    if expect.audio.is_some() {
        streams.push("a:0");
    }
    let base = earliest(&clip, &streams);
    let mut report = format!("{name}: movie {:.3} s", movie.duration_secs());
    if let Some((fourcc, (w, h))) = expect.video {
        let v = movie.tracks.iter().find(|t| t.kind == Kind::Video).expect("video track");
        assert_eq!(&v.fourcc, fourcc.as_bytes());
        assert_eq!((v.width, v.height), (w, h), "{name}: size");
        let n = check_track(&clip, v, "v:0", base, Bytes::Same, 0);
        match &v.codec {
            Codec::H264 { .. } | Codec::Hevc { .. } => check_nal_lengths(&clip, v),
            _ => {}
        }
        report += &format!(", video {n} packets ({} sync)", v.samples.iter().filter(|s| s.key).count());
    }
    if let Some((is_codec, rate, channels, bytes, tol)) = expect.audio {
        let a = movie.tracks.iter().find(|t| t.kind == Kind::Audio && !t.samples.is_empty()).expect("audio track");
        assert!(is_codec(&a.codec), "{name}: audio codec {:?}", a.codec);
        assert_eq!((a.sample_rate, a.channels), (rate, channels), "{name}: audio format");
        let n = check_track(&clip, a, "a:0", base, bytes, tol);
        report += &format!(", audio {n} packets");
    }
    assert!((movie.duration_secs() - 3.0).abs() < 0.2, "{name}: duration {}", movie.duration_secs());
    eprintln!("{report}: all match ffprobe");
}

fn is_aac(c: &Codec) -> bool {
    matches!(c, Codec::Aac { .. })
}

#[test]
fn h264_aac_ts() {
    check_clip("t_h264_aac.ts", H264, AAC, TS, Expect { video: Some(("avc1", (640, 360))), audio: Some((is_aac, 48000, 1, Bytes::Header(7), 1)) });
}

fn is_codec(want: &'static str) -> fn(&Codec) -> bool {
    match want {
        "hevc" => |c| matches!(c, Codec::Hevc { .. }),
        "ac3" => |c| matches!(c, Codec::Ac3 { .. }),
        "eac3" => |c| matches!(c, Codec::Eac3 { .. }),
        "mp3" => |c| matches!(c, Codec::Mp3),
        "mp2" => |c| matches!(c, Codec::Other),
        "pcm16" => |c| matches!(c, Codec::Pcm { bits: 16, float: false, big_endian: true }),
        "pcm24" => |c| matches!(c, Codec::Pcm { bits: 24, float: false, big_endian: true }),
        _ => unreachable!(),
    }
}

const V: Option<(&str, (u32, u32))> = Some(("avc1", (640, 360)));

#[test]
fn hevc_aac_ts() {
    check_clip("t_hevc_aac.ts", HEVC, AAC, TS, Expect { video: Some(("hvc1", (640, 360))), audio: Some((is_aac, 48000, 1, Bytes::Header(7), 1)) });
}

#[test]
fn h264_ac3_m2ts() {
    let a = &["-c:a", "ac3", "-b:a", "192k"];
    check_clip("t_h264_ac3.m2ts", H264, a, M2TS, Expect { video: V, audio: Some((is_codec("ac3"), 48000, 1, Bytes::Same, 1)) });
}

#[test]
fn h264_eac3_ts() {
    let a = &["-c:a", "eac3", "-b:a", "192k", "-ac", "2"];
    check_clip("t_h264_eac3.ts", H264, a, TS, Expect { video: V, audio: Some((is_codec("eac3"), 48000, 2, Bytes::Same, 1)) });
}

#[test]
fn h264_pcm_bluray_m2ts() {
    let a = &["-c:a", "pcm_bluray", "-ac", "2", "-sample_fmt", "s16"];
    check_clip("t_h264_lpcm16.m2ts", H264, a, M2TS, Expect { video: V, audio: Some((is_codec("pcm16"), 48000, 2, Bytes::Header(4), 1)) });
    let a = &["-c:a", "pcm_bluray", "-ac", "2", "-sample_fmt", "s32"];
    check_clip("t_h264_lpcm24.m2ts", H264, a, M2TS, Expect { video: V, audio: Some((is_codec("pcm24"), 48000, 2, Bytes::Header(4), 1)) });
}

#[test]
fn h264_mp3_ts() {
    let a = &["-c:a", "libmp3lame", "-b:a", "128k"];
    check_clip("t_h264_mp3.ts", H264, a, TS, Expect { video: V, audio: Some((is_codec("mp3"), 48000, 1, Bytes::Same, 1)) });
}

/// Program streams: the audio is read (frames, times, bytes against ffprobe), the MPEG video track
/// is cut into pictures (sizes, bytes, times and key frames against ffprobe).
fn check_ps(name: &str, video: &[&str], audio: &[&str], mux: &[&str], mpeg2: bool, a_codec: fn(&Codec) -> bool, a_channels: u16) {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg not found");
        return;
    }
    let clip = make_av(name, video, audio, mux);
    let movie = rustdecoder::container::read(&mut File::open(&clip).unwrap()).unwrap();
    let v = movie.tracks.iter().find(|t| t.kind == Kind::Video).expect("video track listed");
    assert_eq!(&v.fourcc, if mpeg2 { b"mpg2" } else { b"mpg1" });
    assert_eq!((v.width, v.height), (640, 360));
    assert!(!v.samples.is_empty() && movie.video().is_some());
    let a = movie.tracks.iter().find(|t| t.kind == Kind::Audio).expect("audio track");
    assert!(a_codec(&a.codec), "{name}: {:?}", a.codec);
    assert_eq!((a.sample_rate, a.channels), (48000, a_channels));
    let base = earliest(&clip, &["v:0", "a:0"]);
    let nv = check_track(&clip, v, "v:0", base, Bytes::Same, 1);
    let n = check_track(&clip, a, "a:0", base, Bytes::Same, 1);
    eprintln!("{name}: {nv} video pictures ({:?}) and {n} audio packets match ffprobe", std::str::from_utf8(&v.fourcc).unwrap());
}

#[test]
fn mpeg2_ac3_vob() {
    let v = &["-c:v", "mpeg2video", "-pix_fmt", "yuv420p", "-g", "12", "-bf", "2"];
    check_ps("p_mpeg2_ac3.vob", v, &["-c:a", "ac3", "-b:a", "192k"], &["-f", "vob"], true, is_codec("ac3"), 1);
}

#[test]
fn mpeg2_mp2_mpg() {
    let v = &["-c:v", "mpeg2video", "-pix_fmt", "yuv420p", "-g", "12"];
    check_ps("p_mpeg2_mp2.mpg", v, &["-c:a", "mp2", "-b:a", "128k"], &["-f", "mpeg"], true, is_codec("mp2"), 1);
}

#[test]
fn mpeg1_mp2_mpg() {
    let v = &["-c:v", "mpeg1video", "-pix_fmt", "yuv420p", "-g", "12"];
    check_ps("p_mpeg1_mp2.mpg", v, &["-c:a", "mp2", "-b:a", "128k"], &["-f", "mpeg", "-muxrate", "2000000"], false, is_codec("mp2"), 1);
}

#[test]
fn mpeg2_mp2_ts_is_listed() {
    let v = &["-c:v", "mpeg2video", "-pix_fmt", "yuv420p", "-g", "12", "-bf", "2"];
    let a = &["-c:a", "mp2", "-b:a", "128k"];
    check_clip("t_mpeg2_mp2.ts", v, a, TS, Expect { video: Some(("mpg2", (640, 360))), audio: Some((is_codec("mp2"), 48000, 1, Bytes::Same, 1)) });
}

#[cfg(windows)]
mod decode {
    use super::*;

    fn decode(name: &str, video: &[&str], audio: &[&str], mux: &[&str]) {
        if !have_ffmpeg() {
            eprintln!("skipped: ffmpeg not found");
            return;
        }
        let clip = make_av(name, video, audio, mux);
        let Some((n, first)) = ffcompare::compare_decode(&clip, 100_000) else { return };
        eprintln!("{name}: {n} frames decoded on the GPU and compared with ffmpeg: {first:?}");
        assert_eq!(n, 90, "{name}: frame count");
        assert_eq!(first, None, "{name}");
    }

    #[test]
    fn h264_aac_ts() {
        decode("t_h264_aac.ts", H264, AAC, TS);
    }

    #[test]
    fn h264_ac3_m2ts() {
        decode("t_h264_ac3.m2ts", H264, &["-c:a", "ac3", "-b:a", "192k"], M2TS);
    }

    #[test]
    fn hevc_aac_ts() {
        decode("t_hevc_aac.ts", HEVC, AAC, TS);
    }
}
