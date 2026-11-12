//! HEVC parameter sets read with tracing on, compared element by element with ffmpeg's
//! `trace_headers` output for the same NAL units (tests/data/*.trace: "name value" lines).

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::path::{Path, PathBuf};

use rustdecoder::bits::BitReader;
use rustdecoder::mp4::{self, Codec};
use rustdecoder::{hevc, nal};

fn data(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(name)
}

/// Every element we read must be in ffmpeg's trace under the same name, with the same value,
/// in the same order among elements of that name. (ffmpeg also lists reserved bits we skip.)
fn same_as_ffmpeg(ours: &[(String, i64)], trace: &str) {
    let text = std::fs::read_to_string(data(trace)).unwrap();
    let mut theirs: HashMap<&str, VecDeque<i64>> = HashMap::new();
    for line in text.lines() {
        let mut it = line.split(' ');
        let (Some(name), Some(v)) = (it.next(), it.next()) else { continue };
        theirs.entry(name).or_default().push_back(v.parse().unwrap());
    }
    assert!(ours.len() > 20, "too little read: {ours:?}");
    for (name, v) in ours {
        let got = theirs.get_mut(name.as_str()).and_then(|q| q.pop_front());
        assert_eq!(got, Some(*v), "{name} (ffmpeg {got:?}, ours {v}) in {trace}");
    }
}

fn hvcc_of(mp4_name: &str) -> nal::Config {
    let m = mp4::read(&mut File::open(data(mp4_name)).unwrap()).unwrap();
    let Codec::Hevc { config } = &m.video().unwrap().codec else { panic!("not HEVC") };
    nal::hvcc(config).unwrap()
}

fn check(cfg: &nal::Config, sps_trace: &str, pps_trace: &str) -> (hevc::Sps, hevc::Pps) {
    let unit = |t: u8| cfg.units.iter().find(|u| hevc::nal_unit_type(u) == Some(t)).unwrap();
    let rbsp = nal::unescape(unit(hevc::nal_type::SPS));
    let mut b = BitReader::traced(&rbsp);
    hevc::nal_header(&mut b).unwrap();
    let sps = hevc::sps(&mut b).unwrap();
    same_as_ffmpeg(&b.take_trace(), sps_trace);

    let rbsp = nal::unescape(unit(hevc::nal_type::PPS));
    let mut b = BitReader::traced(&rbsp);
    hevc::nal_header(&mut b).unwrap();
    let pps = hevc::pps(&mut b).unwrap();
    same_as_ffmpeg(&b.take_trace(), pps_trace);
    (sps, pps)
}

#[test]
fn x265_main10() {
    let cfg = hvcc_of("hevc10.mp4");
    assert_eq!(cfg.len_size, 4);
    let (sps, pps) = check(&cfg, "hevc10.sps.trace", "hevc10.pps.trace");
    assert_eq!((sps.width, sps.height, sps.bit_depth_luma, sps.chroma_format_idc), (64, 48, 10, 1));
    assert_eq!(sps.profile.profile_idc, 2);
    assert_eq!(sps.vui.timing, Some((1001, 30000)));
    assert_eq!(pps.sps_id, sps.id);
}

/// A Sony XAVC HS camera's 4K 59.94 fps Main 10 parameter sets: 17 short-term reference
/// picture sets, HRD parameters, and the camera's own scaling lists in the PPS.
#[test]
fn sony_xavc_hs_4k60() {
    let b = std::fs::read(data("xavc_hs.hvcC")).unwrap();
    let cfg = nal::hvcc(&b).unwrap();
    let (sps, pps) = check(&cfg, "xavc_hs.sps.trace", "xavc_hs.pps.trace");
    assert_eq!((sps.width, sps.height, sps.bit_depth_luma), (3840, 2160, 10));
    assert_eq!(sps.st_rps.len(), 17);
    // set 0 is empty (intra pictures); set 1: one picture before, used
    assert_eq!(sps.st_rps[0].num_delta_pocs(), 0);
    assert_eq!((sps.st_rps[1].delta_poc_s0.as_slice(), sps.st_rps[1].used_s0.as_slice()), (&[-1][..], &[true][..]));
    assert!(sps.scaling_list_enabled && sps.scaling_list.is_none());
    let sl = pps.scaling_list.as_ref().unwrap();
    // scaling_list_delta_coeff[0][0]: 4, 2, 0, 2 from 8 → 12, 14, 14, 16
    assert_eq!(sl.s4[0][..4], [12, 14, 14, 16]);
    assert_eq!(pps.id, 2);
    assert_eq!(sps.vui.timing, Some((4004, 240000)));
}

/// Writes the parameter sets of `RUSTDECODER_SAMPLE` to tests/data/xavc_hs.hvcC (headers only:
/// no picture data).
#[test]
#[ignore]
fn extract_camera_hvcc() {
    let Some(p) = std::env::var_os("RUSTDECODER_SAMPLE") else { return };
    let m = mp4::read(&mut File::open(p).unwrap()).unwrap();
    let Codec::Hevc { config } = &m.video().unwrap().codec else { panic!("not HEVC") };
    std::fs::write(data("xavc_hs.hvcC"), config).unwrap();
}
