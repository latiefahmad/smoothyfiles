//! `H264Decoder`: an MP4 / MOV file with H.264 in, decoded pictures out on the GPU, in display
//! order. The same shape as [`crate::HevcDecoder`].

use std::collections::VecDeque;
use std::fs::File;
use std::path::Path;

use windows::core::GUID;

use crate::bits::BitReader;
use crate::d3d11::{Gpu, HwDecoder, Planes, h264_profile};
use crate::decoder::{Frame, StreamInfo};
use crate::dxva::{self, SliceShort};
use crate::dxva_h264::{self, PicInfo};
use crate::h264::{self, Pps, SliceHeader, Sps, nal_type};
use crate::h264_dpb::{Dpb, Started};
use crate::mp4::{Codec, Track};
use crate::{Error, Result, nal};

/// One picture collected from a sample: its slice NAL units, as stored (escaped).
struct Pending {
    sh: SliceHeader,
    sps: Sps,
    pps: Pps,
    pts: i64,
    intra: bool,
    slices: Vec<Vec<u8>>,
}

struct Active {
    hw: HwDecoder,
    sps: Sps,
    pool: usize,
}

pub struct H264Decoder {
    file: File,
    track: Track,
    len_size: usize,
    sps: Vec<Option<Sps>>,
    pps: Vec<Option<Pps>>,
    gpu: Gpu,
    #[allow(dead_code)]
    profile: GUID,
    active: Option<Active>,
    dpb: Dpb,
    next_sample: usize,
    building: Option<Pending>,
    queue: VecDeque<Pending>,
    flushed: bool,
    feedback: u32,
    info: StreamInfo,
    buf: Vec<u8>,
}

/// What the stream's first SPS says is wrong with it for this decoder, if anything.
fn check_supported(sps: &Sps) -> Result<()> {
    if sps.chroma_format_idc != 1 || sps.separate_colour_plane {
        return Err(Error::Unsupported("H.264 other than 4:2:0"));
    }
    if sps.bit_depth_luma != 8 || sps.bit_depth_chroma != 8 {
        return Err(Error::Unsupported("H.264 other than 8-bit"));
    }
    if sps.transform_bypass {
        return Err(Error::Unsupported("H.264 lossless (qpprime_y_zero_transform_bypass)"));
    }
    Ok(())
}

impl H264Decoder {
    pub fn open(path: &Path) -> Result<Self> {
        let mut file = File::open(path)?;
        let movie = crate::container::read(&mut file)?;
        let track = movie.video().ok_or(Error::Invalid("no video track"))?.clone();
        let Codec::H264 { config } = &track.codec else { return Err(Error::Unsupported("not an H.264 track")) };
        let cfg = nal::avcc(config)?;
        let mut sps = vec![None; 32];
        let mut pps = vec![None; 256];
        for u in &cfg.units {
            Self::param_set(&mut sps, &mut pps, u)?;
        }
        let first = sps.iter().flatten().next().ok_or(Error::Invalid("no SPS in avcC"))?.clone();
        check_supported(&first)?;
        let (profile, format) = h264_profile();
        let gpu = Gpu::open(&profile, format)?;
        let (w, h) = first.display_size();
        let info = StreamInfo { width: w, height: h, bit_depth: 8, frame_count: track.samples.len(), frame_rate: track.frame_rate(), timescale: track.timescale, rotation: track.rotation, display: crate::decoder::display_size(w, h, track.par, (first.sar.0 as u32, first.sar.1 as u32)), vui: Default::default() };
        Ok(Self {
            file,
            len_size: cfg.len_size,
            track,
            sps,
            pps,
            gpu,
            profile,
            active: None,
            dpb: Dpb::new(0),
            next_sample: 0,
            building: None,
            queue: VecDeque::new(),
            flushed: false,
            feedback: 0,
            info,
            buf: Vec::new(),
        })
    }

    fn param_set(sps: &mut [Option<Sps>], pps: &mut [Option<Pps>], unit: &[u8]) -> Result<()> {
        let Some(t) = h264::nal_unit_type(unit) else { return Ok(()) };
        if t != nal_type::SPS && t != nal_type::PPS {
            return Ok(());
        }
        let rbsp = nal::unescape(&unit[1..]);
        let mut b = BitReader::new(&rbsp);
        if t == nal_type::SPS {
            let s = h264::sps(&mut b)?;
            let id = s.id as usize;
            sps[id] = Some(s);
        } else {
            let p = h264::pps(&mut b, |id| sps.get(id as usize).and_then(|s| s.as_ref()).map_or(1, |s| s.chroma_format_idc))?;
            let id = p.id as usize;
            pps[id] = Some(p);
        }
        Ok(())
    }

    pub fn info(&self) -> &StreamInfo {
        &self.info
    }

    /// The device the pictures live on (the player copies and converts on it).
    pub fn gpu(&self) -> &Gpu {
        &self.gpu
    }

    /// The video track with its sample table (presentation times, sync samples).
    pub fn track(&self) -> &Track {
        &self.track
    }

    /// Continues decoding at sample `i`, which must be a sync sample. Everything held for
    /// output is dropped. The next [`H264Decoder::next_frame`] returns pictures from there on.
    pub fn seek_to_sample(&mut self, i: usize) {
        let pool = self.active.as_ref().map_or(0, |a| a.pool);
        self.dpb = Dpb::new(pool);
        self.next_sample = i.min(self.track.samples.len());
        self.building = None;
        self.queue.clear();
        self.flushed = false;
    }

    /// The name of the graphics adapter decoding.
    pub fn adapter(&self) -> &str {
        &self.gpu.adapter_name
    }

    /// ConfigBitstreamRaw of the decoder configuration in use (1: short slice format), once
    /// decoding has started.
    pub fn config_bitstream_raw(&self) -> Option<u32> {
        self.active.as_ref().map(|a| a.hw.config_bitstream_raw)
    }

    /// Waits until the GPU has finished all pictures submitted (for timing).
    pub fn wait_idle(&self) -> Result<()> {
        self.gpu.wait_idle()
    }

    /// The next picture in display order; `None` at the end of the stream.
    pub fn next_frame(&mut self) -> Result<Option<Frame>> {
        self.dpb.release_held();
        loop {
            if let Some(f) = self.dpb.next_output() {
                let a = self.active.as_ref().ok_or(Error::Invalid("frame without decoder"))?;
                let (w, h) = a.sps.display_size();
                return Ok(Some(Frame { index: f.surface as u32, texture: a.hw.texture.clone(), poc: f.poc, pts: f.pts, width: w, height: h, crop: a.sps.crop, bit_depth: 8 }));
            }
            if let Some(p) = self.queue.pop_front() {
                if !self.matches(&p.sps) {
                    if self.active.is_some() {
                        self.dpb.flush();
                        if self.dpb.has_output() {
                            self.queue.push_front(p);
                            continue;
                        }
                    }
                    self.init(&p.sps)?;
                }
                self.decode(p)?;
                continue;
            }
            if self.next_sample < self.track.samples.len() {
                self.read_sample()?;
                continue;
            }
            if !self.flushed {
                self.flushed = true;
                self.dpb.flush();
                continue;
            }
            return Ok(None);
        }
    }

    /// Copies a frame to the CPU (for tests): cropped planes, one `u16` per sample.
    pub fn read_back(&mut self, f: &Frame) -> Result<Planes> {
        let a = self.active.as_mut().ok_or(Error::Invalid("no decoder"))?;
        let coded = a.sps.coded_size();
        a.hw.read_back(&self.gpu, f.index as usize, coded, f.crop)
    }

    fn matches(&self, sps: &Sps) -> bool {
        self.active.as_ref().is_some_and(|a| a.sps.coded_size() == sps.coded_size() && a.sps.crop == sps.crop && sps.dpb_size() + 3 <= a.pool)
    }

    fn init(&mut self, sps: &Sps) -> Result<()> {
        check_supported(sps)?;
        let (profile, format) = h264_profile();
        // the buffer holds dpb_size frames, one is out with the caller, one is being decoded
        let pool = sps.dpb_size() + 3;
        let (w, h) = sps.coded_size();
        let hw = HwDecoder::with_alignment(&self.gpu, &profile, format, w, h, pool, 16, 2)?;
        log::debug!("H.264 decoder on {}: {}x{}, {} surfaces, ConfigBitstreamRaw {}", self.gpu.adapter_name, w, h, pool, hw.config_bitstream_raw);
        self.dpb = Dpb::new(pool);
        self.active = Some(Active { hw, sps: sps.clone(), pool });
        Ok(())
    }

    /// Reads the next sample: parameter sets are taken up, the slices become pending pictures.
    fn read_sample(&mut self) -> Result<()> {
        let i = self.next_sample;
        self.next_sample += 1;
        let pts = self.track.samples[i].pts;
        self.track.read_sample(&mut self.file, i, &mut self.buf)?;
        let buf = std::mem::take(&mut self.buf);
        let r = self.parse_sample(&buf, pts);
        self.buf = buf;
        // a sample is one access unit: its picture is complete
        if let Some(p) = self.building.take() {
            self.queue.push_back(p);
        }
        r
    }

    fn parse_sample(&mut self, sample: &[u8], pts: i64) -> Result<()> {
        for u in nal::units(sample, self.len_size) {
            let u = u?;
            if u.is_empty() {
                continue;
            }
            let Some(t) = h264::nal_unit_type(u) else { continue };
            match t {
                nal_type::SPS | nal_type::PPS => Self::param_set(&mut self.sps, &mut self.pps, u)?,
                nal_type::END_OF_SEQUENCE => self.dpb.end_of_sequence(),
                nal_type::DPA..=nal_type::DPC => return Err(Error::Unsupported("H.264 data partitioning")),
                nal_type::SLICE | nal_type::IDR => {
                    let head = nal::unescape(&u[1..u.len().min(2048)]);
                    let mut b = BitReader::new(&head);
                    let sh = {
                        let (pps, sps) = (&self.pps, &self.sps);
                        h264::slice_header(&mut b, t, h264::nal_ref_idc(u), |id| pps.get(id as usize).and_then(|p| p.as_ref()), |id| sps.get(id as usize).and_then(|s| s.as_ref()))?
                    };
                    if sh.redundant_pic_cnt > 0 {
                        continue;
                    }
                    if let Some(p) = &mut self.building {
                        if !h264::is_new_picture(&p.sh, &sh) {
                            p.intra &= sh.is_intra();
                            p.slices.push(u.to_vec());
                            continue;
                        }
                        let done = self.building.take().unwrap();
                        self.queue.push_back(done);
                    }
                    let pps = self.pps[sh.pps_id as usize].clone().ok_or(Error::Invalid("missing PPS"))?;
                    let sps = self.sps[pps.sps_id as usize].clone().ok_or(Error::Invalid("missing SPS"))?;
                    let intra = sh.is_intra();
                    self.building = Some(Pending { sh, sps, pps, pts, intra, slices: vec![u.to_vec()] });
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn decode(&mut self, p: Pending) -> Result<()> {
        let Some(started): Option<Started> = self.dpb.start_picture(&p.sh, &p.sps)? else { return Ok(()) };
        // the second field of a frame is decoded into the surface of the first
        let surface = match started.second_of {
            Some(s) => s,
            None => self.dpb.alloc().ok_or(Error::Invalid("no free surface"))?,
        };
        self.feedback = self.feedback.wrapping_add(1).max(1);
        let refs = self.dpb.ref_pics();
        let pp = dxva_h264::pic_params(&PicInfo { sps: &p.sps, pps: &p.pps, sh: &p.sh, started: &started, intra: p.intra, surface, refs: &refs, feedback: self.feedback });
        let qm = dxva_h264::qmatrix(&h264::matrices(&p.sps, &p.pps));

        let mut bitstream = Vec::with_capacity(p.slices.iter().map(|s| s.len() + 3).sum());
        let mut slices = Vec::with_capacity(p.slices.len());
        for s in &p.slices {
            slices.push(SliceShort { bs_nal_unit_data_location: bitstream.len() as u32, slice_bytes_in_buffer: s.len() as u32 + 3, bad_slice_chopping: 0 });
            bitstream.extend_from_slice(&[0, 0, 1]);
            bitstream.extend_from_slice(s);
        }
        let a = self.active.as_mut().ok_or(Error::Invalid("no decoder"))?;
        a.hw.decode(&self.gpu, surface, dxva::bytes_of(&pp), Some(dxva::bytes_of(&qm)), &mut slices, &bitstream)?;
        self.dpb.finish_picture(surface, &started, &p.sh, &p.sps, p.pts)
    }
}
