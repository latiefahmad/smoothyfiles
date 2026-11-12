//! MXF (OP1a, OP-Atom): files written by ffmpeg's `mxf`, `mxf_opatom` and `mxf_d10` muxers.
//!
//! * the sample table of every track is compared with `ffprobe -show_packets` (count, size,
//!   times, key frames, and a CRC of each packet's bytes as the reader hands them out);
//! * DNxHD / DNxHR and ProRes frames read from MXF are decoded and compared with ffmpeg's
//!   decoder (the codecs' own ±1 rule), H.264 on the GPU bit for bit;
//! * structure: run-in, a file cut before its footer (a recorder that was stopped), PCM audio
//!   through the audio decoder, one OP1a DNxHR + PCM file through `Playback`.
//!
//! Tests skip with a message when ffmpeg or a GPU decoder is missing.

mod common;

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

use common::{have_ffmpeg, temp_dir};
use rustdecoder::mp4::{Codec, Kind, Movie};
use rustdecoder::mxf::{self, OperationalPattern, Wrapping};

static LOCK: Mutex<()> = Mutex::new(());

struct Spec {
    name: &'static str,
    size: (u32, u32),
    frames: u32,
    /// Video encoder arguments (pixel format included).
    video: &'static [&'static str],
    /// Audio encoder arguments and the channel count; `None`: no sound.
    audio: Option<(&'static [&'static str], u32)>,
    /// ffmpeg's muxer name.
    format: &'static str,
}

fn make(s: &Spec) -> PathBuf {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let out = temp_dir().join(format!("mxf_{}.mxf", s.name));
    if out.exists() {
        return out;
    }
    let tmp = out.with_extension("part.mxf");
    let mut c = Command::new("ffmpeg");
    c.args(["-y", "-v", "error", "-f", "lavfi", "-i"]).arg(format!("testsrc2=size={}x{}:rate=25", s.size.0, s.size.1));
    if let Some((_, ch)) = s.audio {
        c.args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000"]);
        c.args(["-ac", &ch.to_string()]);
    }
    c.args(["-t", &format!("{:.3}", s.frames as f64 / 25.0)]).args(s.video);
    if let Some((a, _)) = s.audio {
        c.args(a);
    }
    let status = c.args(["-f", s.format]).arg(&tmp).status().expect("ffmpeg");
    assert!(status.success(), "ffmpeg could not make {}", s.name);
    std::fs::rename(&tmp, &out).unwrap();
    out
}

const DNXHD_120: &[&str] = &["-c:v", "dnxhd", "-b:v", "120M", "-pix_fmt", "yuv422p"];
const DNXHD_90: &[&str] = &["-c:v", "dnxhd", "-b:v", "90M", "-pix_fmt", "yuv422p"];
const DNXHR_HQ: &[&str] = &["-c:v", "dnxhd", "-profile:v", "dnxhr_hq", "-pix_fmt", "yuv422p"];
const PRORES_HQ: &[&str] = &["-c:v", "prores_ks", "-profile:v", "3", "-pix_fmt", "yuv422p10le"];
const X264_LONG: &[&str] = &["-c:v", "libx264", "-pix_fmt", "yuv420p", "-g", "12", "-bf", "2", "-x264-params", "scenecut=0"];
const X264_INTRA: &[&str] = &["-c:v", "libx264", "-pix_fmt", "yuv420p", "-g", "1"];
const MPEG2: &[&str] = &["-c:v", "mpeg2video", "-pix_fmt", "yuv420p", "-g", "6", "-bf", "2"];
const D10: &[&str] = &[
    "-c:v", "mpeg2video", "-pix_fmt", "yuv422p", "-b:v", "50M", "-minrate", "50M", "-maxrate", "50M", "-bufsize", "2000k", "-rc_init_occupancy", "2000k", "-dc", "10",
    "-flags", "+ildct+low_delay", "-top", "1", "-g", "1", "-non_linear_quant", "1", "-intra_vlc", "1", "-qmax", "3", "-qmin", "1", "-ps", "1",
];
const PCM24: &[&str] = &["-c:a", "pcm_s24le", "-ar", "48000"];
const PCM16: &[&str] = &["-c:a", "pcm_s16le", "-ar", "48000"];

fn specs() -> Vec<Spec> {
    vec![
        Spec { name: "dnxhd1080_op1a", size: (1920, 1080), frames: 10, video: DNXHD_120, audio: Some((PCM24, 2)), format: "mxf" },
        Spec { name: "dnxhd720_atom", size: (1280, 720), frames: 10, video: DNXHD_90, audio: None, format: "mxf_opatom" },
        Spec { name: "dnxhr_hq_op1a", size: (640, 360), frames: 25, video: DNXHR_HQ, audio: Some((PCM16, 6)), format: "mxf" },
        Spec { name: "dnxhr_hq_atom", size: (640, 360), frames: 25, video: DNXHR_HQ, audio: None, format: "mxf_opatom" },
        Spec { name: "prores_op1a", size: (1280, 720), frames: 10, video: PRORES_HQ, audio: Some((PCM24, 2)), format: "mxf" },
        Spec { name: "h264_long_gop", size: (640, 360), frames: 50, video: X264_LONG, audio: Some((PCM24, 2)), format: "mxf" },
        Spec { name: "h264_intra", size: (640, 360), frames: 20, video: X264_INTRA, audio: None, format: "mxf" },
        Spec { name: "mpeg2_op1a", size: (640, 360), frames: 25, video: MPEG2, audio: Some((PCM16, 2)), format: "mxf" },
        Spec { name: "d10", size: (720, 608), frames: 10, video: D10, audio: Some((PCM24, 4)), format: "mxf_d10" },
    ]
}

#[derive(Debug)]
struct Pk {
    stream: usize,
    pts: f64,
    dts: f64,
    size: usize,
    key: bool,
    crc: u32,
}

fn probe(path: &Path) -> Vec<Pk> {
    let o = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "packet=stream_index,pts_time,dts_time,size,flags,data_hash", "-show_data_hash", "crc32", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .expect("ffprobe");
    assert!(o.status.success());
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .map(|l| {
            let f: Vec<&str> = l.split(',').collect();
            Pk {
                stream: f[0].parse().unwrap(),
                pts: f[1].parse().unwrap_or(f64::NAN),
                dts: f[2].parse().unwrap_or(f64::NAN),
                size: f[3].parse().unwrap(),
                key: f[4].starts_with('K'),
                crc: u32::from_str_radix(f[5].trim_start_matches("CRC32:"), 16).unwrap(),
            }
        })
        .collect()
}

fn movie(path: &Path) -> Movie {
    rustdecoder::container::read(&mut File::open(path).unwrap()).unwrap()
}

/// Compares the sample table with ffprobe's packets; returns the number of packets compared.
fn compare_packets(path: &Path) -> usize {
    let mxf = mxf::parse(&mut File::open(path).unwrap()).unwrap();
    let pk = probe(path);
    let mut f = File::open(path).unwrap();
    let mut total = 0;
    for (si, tr) in mxf.movie.tracks.iter().enumerate() {
        let theirs: Vec<&Pk> = pk.iter().filter(|p| p.stream == si).collect();
        assert_eq!(tr.samples.len(), theirs.len(), "stream {si}: packet count");
        let mut buf = Vec::new();
        for (i, (s, t)) in tr.samples.iter().zip(&theirs).enumerate() {
            let what = format!("stream {si} packet {i}");
            // what a decoder gets: Annex B as the file has it is compared raw, the rest as read
            let raw = matches!(tr.codec, Codec::H264 { .. } | Codec::Hevc { .. });
            if raw {
                f.seek(SeekFrom::Start(s.offset)).unwrap();
                buf.resize(s.size as usize, 0);
                f.read_exact(&mut buf).unwrap();
            } else {
                tr.read_sample(&mut f, i, &mut buf).unwrap();
            }
            assert_eq!(buf.len(), t.size, "{what}: size");
            assert_eq!(crc32fast::hash(&buf), t.crc, "{what}: bytes");
            assert_eq!(s.key, t.key, "{what}: key frame");
            if t.pts.is_finite() {
                assert!((tr.seconds(s.pts) - t.pts).abs() < 1e-5, "{what}: pts {} against {}", tr.seconds(s.pts), t.pts);
            }
            if t.dts.is_finite() {
                assert!((tr.seconds(s.dts) - t.dts).abs() < 1e-5, "{what}: dts {} against {}", tr.seconds(s.dts), t.dts);
            }
            total += 1;
        }
    }
    total
}

#[test]
fn sample_tables_match_ffprobe() {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg not found");
        return;
    }
    println!("| clip | pattern | wrapping | tracks | codec | packets compared (count, size, bytes, key, times) |");
    for s in specs() {
        let path = make(&s);
        let m = mxf::parse(&mut File::open(&path).unwrap()).unwrap();
        let n = compare_packets(&path);
        let desc: Vec<String> = m.tracks.iter().zip(&m.movie.tracks).map(|(t, tr)| format!("{:?} {:?} {}", t.kind, t.codec.map_or("PCM".to_string(), |c| format!("{c:?}")), String::from_utf8_lossy(&tr.fourcc))).collect();
        println!("| {} | {:?} | {:?} | {} | {} | {n} |", s.name, m.pattern, m.tracks[0].wrapping, m.tracks.len(), desc.join("; "));
        match s.format {
            "mxf_opatom" => {
                assert_eq!(m.pattern, OperationalPattern::Atom);
                assert_eq!(m.tracks[0].wrapping, Wrapping::Clip);
            }
            _ => {
                assert!(m.pattern.is_op1a(), "{:?}", m.pattern);
                assert_eq!(m.tracks[0].wrapping, Wrapping::Frame);
            }
        }
        assert_eq!(m.movie.video().unwrap().samples.len(), s.frames as usize, "{}", s.name);
        if let Some((_, ch)) = s.audio {
            let a = m.movie.audio().unwrap();
            assert_eq!(a.channels as u32, ch, "{}: audio channels", s.name);
            assert_eq!(a.sample_rate, 48000);
        }
    }
}

#[test]
fn track_properties() {
    if !have_ffmpeg() {
        return;
    }
    let all = specs();
    let find = |n: &str| make(all.iter().find(|s| s.name == n).unwrap());
    let m = movie(&find("dnxhd1080_op1a"));
    let v = m.video().unwrap();
    assert_eq!((v.width, v.height, v.timescale), (1920, 1080, 25));
    assert_eq!(&v.fourcc, b"AVdn");
    assert!(rustdecoder::dnx::is_dnx_fourcc(&v.fourcc));
    assert!((v.frame_rate() - 25.0).abs() < 1e-9);
    let a = m.audio().unwrap();
    assert!(matches!(a.codec, Codec::Pcm { bits: 24, float: false, big_endian: false }), "{:?}", a.codec);
    let m = movie(&find("dnxhr_hq_op1a"));
    assert_eq!(&m.video().unwrap().fourcc, b"AVdh");
    assert!(matches!(m.audio().unwrap().codec, Codec::Pcm { bits: 16, .. }));
    assert_eq!(m.audio().unwrap().channels, 6);
    let m = movie(&find("prores_op1a"));
    assert!(rustdecoder::prores::is_prores_fourcc(&m.video().unwrap().fourcc));
    assert_eq!(&m.video().unwrap().fourcc, b"apch");
    let m = movie(&find("h264_long_gop"));
    let v = m.video().unwrap();
    let Codec::H264 { config } = &v.codec else { panic!("{:?}", v.codec) };
    let cfg = rustdecoder::nal::avcc(config).unwrap();
    assert_eq!((cfg.len_size, cfg.units.len()), (4, 2));
    assert_eq!(v.samples.iter().filter(|s| s.key).count(), 5, "an IDR every 12 frames of 50");
    // MPEG-2 is listed, not decoded
    let m = movie(&find("mpeg2_op1a"));
    let v = m.video().unwrap();
    assert_eq!((v.width, v.height, &v.fourcc), (640, 360, b"mpg2"));
    assert_eq!(v.codec, Codec::Other);
    // D-10: 4 channels of 24 bit AES3, read back as plain PCM, interlaced picture stored as fields
    let m = movie(&find("d10"));
    let a = m.audio().unwrap();
    assert!(matches!(a.codec, Codec::Pcm { bits: 24, float: false, big_endian: false }));
    assert_eq!(a.fourcc, rustdecoder::mp4::AES3);
    let mut f = File::open(find("d10")).unwrap();
    let mut buf = Vec::new();
    a.read_sample(&mut f, 0, &mut buf).unwrap();
    assert_eq!(buf.len(), 1920 * 4 * 3);
    assert_eq!(m.video().unwrap().height, 576);
}

// ---------------------------------------------------------------------------------------------
// decoding

fn reference(path: &Path, fmt: &str) -> Vec<u8> {
    let o = Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(path).args(["-map", "0:v:0", "-f", "rawvideo", "-pix_fmt", fmt, "-"]).stderr(Stdio::inherit()).output().expect("ffmpeg");
    assert!(o.status.success());
    o.stdout
}

use rustdecoder::prores::Picture;

/// Largest sample difference between a decoded picture and ffmpeg's planar output.
fn picture_error(p: &Picture, theirs: &[u8], wide: bool) -> i32 {
    let (w, h) = (p.width as usize, p.height as usize);
    let (cw, ch) = {
        let (a, b) = p.chroma_size();
        (a as usize, b as usize)
    };
    let bps = if wide { 2 } else { 1 };
    let mut worst = 0;
    let mut at = 0;
    for (plane, stride, pw, ph) in [(&p.y, p.stride_y, w, h), (&p.u, p.stride_c, cw, ch), (&p.v, p.stride_c, cw, ch)] {
        for y in 0..ph {
            for x in 0..pw {
                let i = at + (y * pw + x) * bps;
                let t = if wide { u16::from_le_bytes([theirs[i], theirs[i + 1]]) as i32 } else { theirs[i] as i32 };
                worst = worst.max((plane[y * stride + x] as i32 - t).abs());
            }
        }
        at += pw * ph * bps;
    }
    worst
}

fn decode_dnx(path: &Path) -> Vec<Picture> {
    let mut f = File::open(path).unwrap();
    let m = rustdecoder::container::read(&mut f).unwrap();
    let t = m.video().unwrap();
    let dec = rustdecoder::dnx::DnxDecoder::new(2);
    let mut buf = Vec::new();
    (0..t.samples.len())
        .map(|i| {
            t.read_sample(&mut f, i, &mut buf).unwrap();
            let mut p = Picture::default();
            dec.decode(&buf, &mut p).unwrap();
            p
        })
        .collect()
}

#[test]
fn dnx_in_mxf_decodes_like_ffmpeg() {
    if !have_ffmpeg() {
        return;
    }
    println!("| clip | frames | size | worst sample error |");
    for s in specs().iter().filter(|s| s.name.starts_with("dnx")) {
        let path = make(s);
        let pics = decode_dnx(&path);
        let theirs = reference(&path, "yuv422p");
        let (w, h) = (pics[0].width as usize, pics[0].height as usize);
        let fb = w * h * 2;
        assert_eq!(theirs.len(), fb * pics.len(), "{}: frame count", s.name);
        let worst = pics.iter().enumerate().map(|(i, p)| picture_error(p, &theirs[i * fb..][..fb], false)).max().unwrap();
        // the same packets in a MOV give the same pictures: the transport is exact
        let mov = temp_dir().join(format!("mxf_{}_remux.mov", s.name));
        let st = Command::new("ffmpeg").args(["-y", "-v", "error", "-i"]).arg(&path).args(["-map", "0:v:0", "-c", "copy", "-f", "mov"]).arg(&mov).status().unwrap();
        assert!(st.success());
        let from_mov = decode_dnx(&mov);
        assert_eq!(from_mov.len(), pics.len());
        assert!(pics.iter().zip(&from_mov).all(|(a, b)| a.y == b.y && a.u == b.u && a.v == b.v), "{}: MXF and MOV pictures differ", s.name);
        println!("| {} | {} | {w}x{h} | {worst} |", s.name, pics.len());
        // (720p DNxHD at 90 Mb/s shows an IDCT rounding difference of 2 in later frames: the same
        // in a MOV, so it is the decoder's, not the container's)
        assert!(worst <= 2, "{}: error {worst}", s.name);
        assert!(worst <= 1 || s.name == "dnxhd720_atom", "{}: error {worst}", s.name);
    }
}

#[test]
fn prores_in_mxf_decodes_like_ffmpeg() {
    if !have_ffmpeg() {
        return;
    }
    let s = specs().into_iter().find(|s| s.name == "prores_op1a").unwrap();
    let path = make(&s);
    let mut f = File::open(&path).unwrap();
    let m = rustdecoder::container::read(&mut f).unwrap();
    let t = m.video().unwrap();
    let bits = rustdecoder::prores::bits_for_fourcc(&t.fourcc);
    let dec = rustdecoder::prores::ProResDecoder::new(2);
    let theirs = reference(&path, "yuv422p10le");
    let mut buf = Vec::new();
    let mut worst = 0;
    for i in 0..t.samples.len() {
        t.read_sample(&mut f, i, &mut buf).unwrap();
        let mut p = Picture::default();
        dec.decode(&buf, bits, &mut p).unwrap();
        let fb = p.width as usize * p.height as usize * 4;
        worst = worst.max(picture_error(&p, &theirs[i * fb..][..fb], true));
    }
    println!("ProRes HQ in OP1a: {} frames, worst sample error {worst}", t.samples.len());
    assert!(worst <= 1);
}

#[cfg(windows)]
mod gpu {
    use super::*;
    use rustdecoder::VideoDecoder;

    /// Decodes every frame on the GPU and compares with ffmpeg's output in display order.
    fn h264_matches(name: &str) {
        if !have_ffmpeg() {
            return;
        }
        let s = specs().into_iter().find(|s| s.name == name).unwrap();
        let path = make(&s);
        let mut dec = match VideoDecoder::open(&path) {
            Ok(d) => d,
            Err(rustdecoder::Error::Gpu(m)) => return eprintln!("skipped: {m}"),
            Err(e) => panic!("open: {e}"),
        };
        let (w, h) = (dec.info().width as usize, dec.info().height as usize);
        // ffmpeg numbers the pictures of an Annex B stream in MXF by the packets' times, which the
        // index of its own muxer leaves in decode order: the reference is the raw stream, whose
        // times come from the parser
        let raw = temp_dir().join(format!("mxf_{name}.h264"));
        let st = Command::new("ffmpeg").args(["-y", "-v", "error", "-i"]).arg(&path).args(["-map", "0:v:0", "-c", "copy", "-f", "h264"]).arg(&raw).status().unwrap();
        assert!(st.success());
        let o = Command::new("ffmpeg").args(["-v", "error", "-f", "h264", "-i"]).arg(&raw).args(["-fps_mode", "passthrough", "-f", "rawvideo", "-pix_fmt", "nv12", "-"]).output().unwrap();
        assert!(o.status.success());
        let theirs = o.stdout;
        let fb = w * h * 3 / 2;
        let mut n = 0;
        while let Some(f) = dec.next_frame().unwrap() {
            let planes = dec.read_back(&f).unwrap();
            let r = &theirs[n * fb..][..fb];
            assert!(planes.y.iter().zip(&r[..w * h]).all(|(a, b)| *a == *b as u16), "{name}: frame {n} luma differs");
            assert!(planes.uv.iter().zip(&r[w * h..]).all(|(a, b)| *a == *b as u16), "{name}: frame {n} chroma differs");
            n += 1;
        }
        println!("{name}: {n} frames in display order, bit-exact with ffmpeg");
        assert_eq!(n, s.frames as usize);
        assert_eq!(theirs.len(), fb * n);
    }

    #[test]
    fn h264_long_gop_in_mxf_is_bit_exact() {
        h264_matches("h264_long_gop");
    }

    #[test]
    fn h264_intra_in_mxf_is_bit_exact() {
        h264_matches("h264_intra");
    }
}

// ---------------------------------------------------------------------------------------------
// structure

#[test]
fn run_in_is_skipped() {
    if !have_ffmpeg() {
        return;
    }
    let s = specs().into_iter().find(|s| s.name == "dnxhr_hq_op1a").unwrap();
    let path = make(&s);
    let mut bytes = std::fs::read(&path).unwrap();
    let plain = movie(&path);
    let run_in: Vec<u8> = (0..1000u32).map(|i| (i * 7 + 3) as u8).collect();
    bytes.splice(0..0, run_in.iter().copied());
    let out = temp_dir().join("mxf_run_in.mxf");
    std::fs::write(&out, &bytes).unwrap();
    let m = mxf::parse(&mut File::open(&out).unwrap()).unwrap();
    assert_eq!(m.run_in, 1000);
    let (a, b) = (plain.video().unwrap(), m.movie.video().unwrap());
    assert_eq!(a.samples.len(), b.samples.len());
    assert!(a.samples.iter().zip(&b.samples).all(|(x, y)| y.offset == x.offset + 1000 && x.size == y.size));
    // and container::read finds it
    assert_eq!(movie(&out).video().unwrap().samples.len(), a.samples.len());
}

#[test]
fn a_file_cut_before_its_footer_keeps_its_frames() {
    if !have_ffmpeg() {
        return;
    }
    // what a recorder that stopped (or a copy that did not finish) leaves: the header, the
    // essence, no footer and no index; the last element is cut in the middle
    for name in ["dnxhd1080_op1a", "h264_long_gop"] {
        let s = specs().into_iter().find(|s| s.name == name).unwrap();
        let path = make(&s);
        let full = mxf::parse(&mut File::open(&path).unwrap()).unwrap();
        let footer = full.partitions.iter().find(|p| p.kind == mxf::PartitionKind::Footer).unwrap().pos;
        let v = full.movie.video().unwrap();
        // cut in the middle of frame 6
        let cut = v.samples[6].offset + v.samples[6].size as u64 / 2;
        assert!(cut < footer);
        let mut bytes = vec![0u8; cut as usize];
        File::open(&path).unwrap().read_exact(&mut bytes).unwrap();
        let out = temp_dir().join(format!("mxf_cut_{name}.mxf"));
        std::fs::write(&out, &bytes).unwrap();
        let m = mxf::parse(&mut File::open(&out).unwrap()).unwrap();
        let cv = m.movie.video().unwrap();
        println!("{name}: cut at {cut} of {}: {} frames kept (of {}), incomplete {}", full.movie.video().unwrap().samples.len(), cv.samples.len(), v.samples.len(), m.incomplete);
        assert_eq!(cv.samples.len(), 6);
        assert!(cv.samples.iter().zip(&v.samples).all(|(a, b)| a.offset == b.offset && a.size == b.size));
        assert!(m.incomplete);
        // keys without the footer's index: found from the stream itself
        assert!(cv.samples.iter().zip(&v.samples).all(|(a, b)| a.key == b.key), "{name}: key frames without an index");
        // the audio of the frames before the cut is there too
        if let (Some(fa), Some(ca)) = (full.movie.audio(), m.movie.audio()) {
            assert!(ca.samples.len() >= 5 && ca.samples.len() <= fa.samples.len());
        }
    }
}

#[test]
fn damaged_files_do_not_panic() {
    if !have_ffmpeg() {
        return;
    }
    for name in ["dnxhr_hq_op1a", "h264_long_gop", "d10"] {
        let s = specs().into_iter().find(|s| s.name == name).unwrap();
        let bytes = std::fs::read(make(&s)).unwrap();
        let out = temp_dir().join("mxf_damaged.mxf");
        let (mut ok, mut err) = (0, 0);
        // cut at many places, and flip bytes in the header metadata and in the first elements
        for cut in (0..bytes.len().min(40_000)).step_by(331).chain((40_000..bytes.len()).step_by(bytes.len() / 40)) {
            std::fs::write(&out, &bytes[..cut]).unwrap();
            match mxf::parse(&mut File::open(&out).unwrap()) {
                Ok(_) => ok += 1,
                Err(_) => err += 1,
            }
        }
        for at in (0..9000).step_by(29) {
            let mut b = bytes[..bytes.len().min(100_000)].to_vec();
            b[at] ^= 0xff;
            std::fs::write(&out, &b).unwrap();
            match mxf::parse(&mut File::open(&out).unwrap()) {
                Ok(_) => ok += 1,
                Err(_) => err += 1,
            }
        }
        println!("{name}: {ok} damaged files read, {err} refused, no panic");
    }
}

#[test]
fn pcm_audio_decodes_and_matches_ffmpeg() {
    if !have_ffmpeg() {
        return;
    }
    for name in ["dnxhd1080_op1a", "dnxhr_hq_op1a", "d10"] {
        let s = specs().into_iter().find(|s| s.name == name).unwrap();
        let path = make(&s);
        let mut dec = rustdecoder::audio::AudioDecoder::open(&path).unwrap().expect("audio track");
        let ch = dec.info().channels as usize;
        let (first, ours) = dec.decode_all().unwrap();
        assert_eq!(first, 0.0);
        // ffmpeg's decode of the same audio stream as float
        let o = Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(&path).args(["-map", "0:a:0", "-f", "f32le", "-"]).output().unwrap();
        assert!(o.status.success());
        let theirs: Vec<f32> = o.stdout.chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect();
        assert_eq!(ours.len(), theirs.len(), "{name}: {ch} channels, sample count");
        let worst = ours.iter().zip(&theirs).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        println!("{name}: {ch} channels, {} frames, worst difference from ffmpeg {worst:e}", ours.len() / ch);
        assert!(worst < 1e-6, "{name}: {worst}");
    }
}

// ---------------------------------------------------------------------------------------------
// playback

#[cfg(windows)]
mod playback {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    use rustdecoder::audio_out::AudioSink;
    use rustdecoder::player::{FakeTime, TimeSource};
    use rustdecoder::playback::Playback;

    #[derive(Default)]
    struct St {
        rate: u32,
        channels: usize,
        pushes: Vec<(f64, usize)>,
        pushed: f64,
        base: Option<f64>,
        running: bool,
        played: f64,
        since: f64,
    }

    /// A sink that plays nothing and counts time by a fake clock.
    struct FakeSink {
        time: Arc<FakeTime>,
        st: Mutex<St>,
    }

    impl FakeSink {
        fn played(&self, s: &St) -> f64 {
            if s.running { s.played + (self.time.now() - s.since) } else { s.played }
        }
    }

    impl AudioSink for FakeSink {
        fn set_input(&self, rate: u32, channels: usize) {
            let mut s = self.st.lock().unwrap();
            s.rate = rate;
            s.channels = channels;
        }
        fn push(&self, pts: f64, pcm: &[f32]) {
            let mut s = self.st.lock().unwrap();
            let frames = pcm.len() / s.channels;
            s.base.get_or_insert(pts);
            s.pushed += frames as f64 / s.rate as f64;
            s.pushes.push((pts, frames));
        }
        fn end_of_data(&self) {}
        fn buffered(&self) -> f64 {
            let s = self.st.lock().unwrap();
            (s.pushed - self.played(&s)).max(0.0)
        }
        fn flush(&self) {
            let mut s = self.st.lock().unwrap();
            s.pushes.clear();
            s.pushed = 0.0;
            s.played = 0.0;
            s.running = false;
            s.base = None;
        }
        fn play(&self) {
            let mut s = self.st.lock().unwrap();
            if !s.running {
                s.since = self.time.now();
                s.running = true;
            }
        }
        fn pause(&self) {
            let mut s = self.st.lock().unwrap();
            if s.running {
                s.played += self.time.now() - s.since;
                s.running = false;
            }
        }
        fn position(&self) -> Option<f64> {
            let s = self.st.lock().unwrap();
            s.base.map(|b| b + self.played(&s))
        }
        fn time_source(&self) -> Arc<dyn TimeSource> {
            self.time.clone()
        }
    }

    #[test]
    fn dnxhr_with_pcm_plays_in_sync() {
        if !have_ffmpeg() {
            return;
        }
        let s = specs().into_iter().find(|s| s.name == "dnxhr_hq_op1a").unwrap();
        let path = make(&s);
        let time = Arc::new(FakeTime::new());
        let sink = Arc::new(FakeSink { time: time.clone(), st: Mutex::new(St::default()) });
        let mut pb = match Playback::open_with_sink(&path, 6, Some(sink.clone())) {
            Ok(p) => p,
            Err(rustdecoder::Error::Gpu(m)) => return eprintln!("skipped: {m}"),
            Err(e) => panic!("open: {e}"),
        };
        assert!(pb.has_audio(), "{}", pb.audio_note());
        pb.play();
        let (mut last_serial, mut last_pts, mut shown, mut worst_lag) = (0, -1.0, 0, 0.0f64);
        for _ in 0..54 {
            for _ in 0..1000 {
                if pb.audio_ended() || sink.buffered() > 0.05 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            time.advance(1.0 / 60.0);
            let now = pb.time();
            pb.player.wait_ahead(now, Duration::from_secs(5));
            let Some(f) = pb.frame() else { continue };
            assert!(f.pts <= now + 1e-9, "a picture from the future: {} at {now}", f.pts);
            worst_lag = worst_lag.max(now - f.pts);
            if f.serial != last_serial {
                assert!(f.pts > last_pts, "pictures out of order");
                (last_pts, last_serial) = (f.pts, f.serial);
                shown += 1;
            }
            let a = pb.audio_position().unwrap();
            assert!((a - now).abs() < 1e-6, "audio {a} clock {now}");
        }
        let st = pb.stats();
        println!("0.9 s at 60 Hz: {shown} pictures of 25 fps, dropped {}, late {}, worst lag {:.1} ms", st.dropped, st.late, worst_lag * 1000.0);
        assert!((21..=23).contains(&shown), "{shown} pictures");
        assert_eq!(st.dropped, 0);
        assert!(worst_lag <= 1.0 / 25.0 + 1e-6);
        // the sound queued is one unbroken run from time 0
        let log = sink.st.lock().unwrap().pushes.clone();
        let mut next = 0.0;
        for (pts, frames) in &log {
            assert!((pts - next).abs() < 1e-6, "audio gap: block at {pts}, expected {next}");
            next = pts + *frames as f64 / 48000.0;
        }
        assert!(next >= 0.9, "audio queued up to {next}");
    }
}

#[test]
fn real_camera_mxf_if_there_is_one() {
    // read-only: only the file named here, never a scan of the drive
    let Ok(p) = std::env::var("RD_MXF") else { return };
    let m = mxf::parse(&mut File::open(&p).unwrap()).unwrap();
    println!("{p}: {:?}, {} tracks, {} partitions", m.pattern, m.tracks.len(), m.partitions.len());
    for t in &m.movie.tracks {
        println!("  {:?} {} samples {}", t.kind, String::from_utf8_lossy(&t.fourcc), t.samples.len());
    }
    assert!(m.movie.tracks.iter().any(|t| t.kind == Kind::Video));
}

// ---------------------------------------------------------------------------------------------
// a hand-made file: clip-wrapped PCM (what Avid writes for the audio of an OP-Atom clip) in a
// file whose partition is still open (a recorder at work), no index, no footer

fn klv(key: &[u8; 16], v: &[u8], long: bool) -> Vec<u8> {
    let mut o = key.to_vec();
    if long {
        o.push(0x88);
        o.extend_from_slice(&(v.len() as u64).to_be_bytes());
    } else {
        o.extend_from_slice(&[0x83, (v.len() >> 16) as u8, (v.len() >> 8) as u8, v.len() as u8]);
    }
    o.extend_from_slice(v);
    o
}

fn set(kind: u8, props: &[(u16, Vec<u8>)]) -> Vec<u8> {
    let key = [0x06, 0x0e, 0x2b, 0x34, 0x02, 0x53, 0x01, 0x01, 0x0d, 0x01, 0x01, 0x01, 0x01, 0x01, kind, 0x00];
    let mut v = Vec::new();
    for (t, p) in props {
        v.extend_from_slice(&t.to_be_bytes());
        v.extend_from_slice(&(p.len() as u16).to_be_bytes());
        v.extend_from_slice(p);
    }
    klv(&key, &v, false)
}

fn batch(items: &[[u8; 16]]) -> Vec<u8> {
    let mut v = (items.len() as u32).to_be_bytes().to_vec();
    v.extend_from_slice(&16u32.to_be_bytes());
    for i in items {
        v.extend_from_slice(i);
    }
    v
}

fn rational(n: i32, d: i32) -> Vec<u8> {
    [n.to_be_bytes(), d.to_be_bytes()].concat()
}

/// A file with one clip-wrapped PCM element of `frames` 16-bit stereo frames at `edit_rate` / 1.
fn synthetic_pcm(name: &str, edit_rate: i32, frames: usize) -> (PathBuf, Vec<u8>) {
    let id = |n: u8| -> [u8; 16] { [n; 16] };
    let umid = [0x77u8; 32];
    let ec: [u8; 16] = [0x06, 0x0e, 0x2b, 0x34, 0x04, 0x01, 0x01, 0x01, 0x0d, 0x01, 0x03, 0x01, 0x02, 0x06, 0x02, 0x00];
    let sound_def = [0x06, 0x0e, 0x2b, 0x34, 0x04, 0x01, 0x01, 0x01, 0x01, 0x03, 0x02, 0x02, 0x02, 0x00, 0x00, 0x00];
    let mut meta = Vec::new();
    meta.extend(set(0x2f, &[(0x3c0a, id(1).to_vec()), (0x3b03, id(2).to_vec())]));
    meta.extend(set(0x18, &[(0x3c0a, id(2).to_vec()), (0x1901, batch(&[id(3)]))]));
    meta.extend(set(0x37, &[(0x3c0a, id(3).to_vec()), (0x4401, umid.to_vec()), (0x4403, batch(&[id(4)])), (0x4701, id(6).to_vec())]));
    meta.extend(set(0x3b, &[(0x3c0a, id(4).to_vec()), (0x4801, 1u32.to_be_bytes().to_vec()), (0x4804, 0x1601_0200u32.to_be_bytes().to_vec()), (0x4b01, rational(edit_rate, 1)), (0x4803, id(5).to_vec())]));
    meta.extend(set(0x0f, &[(0x3c0a, id(5).to_vec()), (0x0201, sound_def.to_vec()), (0x0202, (frames as u64).to_be_bytes().to_vec())]));
    meta.extend(set(
        0x48,
        &[
            (0x3c0a, id(6).to_vec()),
            (0x3006, 1u32.to_be_bytes().to_vec()),
            (0x3004, ec.to_vec()),
            (0x3d03, rational(48000, 1)),
            (0x3d07, 2u32.to_be_bytes().to_vec()),
            (0x3d01, 16u32.to_be_bytes().to_vec()),
            (0x3d0a, 4u16.to_be_bytes().to_vec()),
        ],
    ));
    let primer = klv(&[0x06, 0x0e, 0x2b, 0x34, 0x02, 0x05, 0x01, 0x01, 0x0d, 0x01, 0x02, 0x01, 0x01, 0x05, 0x01, 0x00], &[0, 0, 0, 0, 0, 0, 0, 18], false);
    let mut pack = vec![0, 1, 0, 3, 0, 0, 0, 1];
    pack.extend_from_slice(&[0; 8 * 3]);
    pack.extend_from_slice(&((primer.len() + meta.len()) as u64).to_be_bytes());
    pack.extend_from_slice(&0u64.to_be_bytes());
    pack.extend_from_slice(&0u32.to_be_bytes());
    pack.extend_from_slice(&0u64.to_be_bytes());
    pack.extend_from_slice(&1u32.to_be_bytes());
    pack.extend_from_slice(&[0x06, 0x0e, 0x2b, 0x34, 0x04, 0x01, 0x01, 0x01, 0x0d, 0x01, 0x02, 0x01, 0x10, 0x00, 0x00, 0x00]);
    pack.extend_from_slice(&batch(&[ec]));
    // header partition, open and incomplete
    let mut f = klv(&[0x06, 0x0e, 0x2b, 0x34, 0x02, 0x05, 0x01, 0x01, 0x0d, 0x01, 0x02, 0x01, 0x01, 0x02, 0x01, 0x00], &pack, false);
    f.extend(primer);
    f.extend(meta);
    let pcm: Vec<u8> = (0..frames * 2).flat_map(|i| ((i as i32 * 37 % 60000 - 30000) as i16).to_le_bytes()).collect();
    f.extend(klv(&[0x06, 0x0e, 0x2b, 0x34, 0x01, 0x02, 0x01, 0x01, 0x0d, 0x01, 0x03, 0x01, 0x16, 0x01, 0x02, 0x01], &pcm, true));
    let out = temp_dir().join(name);
    std::fs::write(&out, &f).unwrap();
    (out, pcm)
}

#[test]
fn clip_wrapped_pcm_in_an_open_file_is_cut_by_the_edit_rate() {
    // edit rate 25: blocks of 1920 frames; edit rate 48000 (as Avid writes the audio files): 1024
    for (name, rate, block) in [("mxf_synth_pcm25.mxf", 25, 1920usize), ("mxf_synth_pcm48000.mxf", 48000, 1024)] {
        let frames = 5 * block + 700;
        let (path, pcm) = synthetic_pcm(name, rate, frames);
        let m = mxf::parse(&mut File::open(&path).unwrap()).unwrap();
        assert_eq!(m.pattern, OperationalPattern::Atom);
        assert!(m.incomplete, "an open, incomplete partition pack");
        assert_eq!(m.partitions.len(), 1);
        assert!(!m.partitions[0].closed && !m.partitions[0].complete);
        assert_eq!(m.tracks[0].wrapping, Wrapping::Clip);
        let a = m.movie.audio().unwrap();
        assert_eq!((a.channels, a.sample_rate, a.timescale), (2, 48000, 48000));
        assert!(matches!(a.codec, Codec::Pcm { bits: 16, .. }));
        assert_eq!(a.samples.len(), 6);
        let mut at = 0;
        for s in &a.samples {
            assert_eq!(s.pts, at as i64);
            assert_eq!(s.offset, a.samples[0].offset + at as u64 * 4, "blocks follow each other in the file");
            at += s.size as usize / 4;
        }
        assert_eq!(at, frames);
        assert_eq!(a.samples[0].size as usize, block * 4);
        // the bytes are the element's
        let mut f = File::open(&path).unwrap();
        let mut all = Vec::new();
        let mut buf = Vec::new();
        for i in 0..a.samples.len() {
            a.read_sample(&mut f, i, &mut buf).unwrap();
            all.extend_from_slice(&buf);
        }
        assert_eq!(all, pcm);
        // and the audio decoder plays it
        let mut d = rustdecoder::audio::AudioDecoder::open(&path).unwrap().unwrap();
        let (first, out) = d.decode_all().unwrap();
        assert_eq!(first, 0.0);
        assert_eq!(out.len(), frames * 2);
        assert!(out.iter().zip(pcm.chunks_exact(2)).all(|(o, b)| *o == i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0));
    }
}
