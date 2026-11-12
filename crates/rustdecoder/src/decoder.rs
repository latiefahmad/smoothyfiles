//! `HevcDecoder`: an MP4 / MOV file in, decoded pictures out on the GPU, in display order.

use std::fs::File;
use std::path::Path;

use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::core::GUID;

use crate::bits::BitReader;
use crate::d3d11::{Gpu, HwDecoder, Planes, hevc_profile};
use crate::dpb::{Dpb, PicKind, Started};
use crate::dxva::{self, PicInfo, SliceShort};
use crate::hevc::{self, Pps, Sps, nal_type};
use crate::mp4::{Codec, Track};
use crate::slice::{SliceHeader, slice_header};
use crate::{Error, Result, nal};

/// A decoded picture, still on the GPU: surface `index` of the decoder's texture array. Valid
/// until the next call to [`HevcDecoder::next_frame`].
#[derive(Debug, Clone)]
pub struct Frame {
    /// Slice of the texture array.
    pub index: u32,
    pub texture: ID3D11Texture2D,
    /// Picture order count.
    pub poc: i32,
    /// Presentation time of the sample, in the track's timescale.
    pub pts: i64,
    /// Visible size (after the conformance window).
    pub width: u32,
    pub height: u32,
    /// (left, right, top, bottom) crop in luma samples inside the surface.
    pub crop: (u32, u32, u32, u32),
    pub bit_depth: u8,
}

/// What is known about the stream before decoding.
#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    pub frame_count: usize,
    pub frame_rate: f64,
    pub timescale: u32,
    /// Clockwise degrees (0, 90, 180, 270) to turn the picture for display (`tkhd` matrix).
    pub rotation: u16,
    /// Colour description of the first SPS.
    pub vui: crate::hevc::Vui,
    /// Size to show the picture at, before the rotation: the coded width scaled by the pixel
    /// aspect ratio (the container's, else the stream's own), the coded height. Equal to
    /// `(width, height)` for square pixels.
    pub display: (u32, u32),
}

/// [`StreamInfo::display`]: `container` is the pixel aspect ratio the container states (MP4
/// `pasp`, Matroska display size), `sar` the stream's own (VUI; `(0, 0)` = not stated). The
/// container wins, as it does in ffmpeg.
pub fn display_size(width: u32, height: u32, container: Option<(u32, u32)>, sar: (u32, u32)) -> (u32, u32) {
    let (n, d) = match container {
        Some((n, d)) if n > 0 && d > 0 => (n, d),
        _ if sar.0 > 0 && sar.1 > 0 => sar,
        _ => (1, 1),
    };
    (((width as u64 * n as u64 + d as u64 / 2) / d as u64).max(1) as u32, height)
}

struct Pending {
    kind: PicKind,
    sh: SliceHeader,
    pts: i64,
    /// Slice NAL units, as stored (escaped).
    slices: Vec<Vec<u8>>,
    started: Option<Started>,
}

struct Active {
    hw: HwDecoder,
    sps: Sps,
}

pub struct HevcDecoder {
    file: File,
    track: Track,
    len_size: usize,
    sps: Vec<Option<Sps>>,
    pps: Vec<Option<Pps>>,
    gpu: Gpu,
    profile: GUID,
    format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
    active: Option<Active>,
    dpb: Dpb,
    next_sample: usize,
    pending: Option<Pending>,
    flushed: bool,
    feedback: u32,
    info: StreamInfo,
    buf: Vec<u8>,
}

impl HevcDecoder {
    pub fn open(path: &Path) -> Result<Self> {
        let mut file = File::open(path)?;
        let movie = crate::container::read(&mut file)?;
        let track = movie.video().ok_or(Error::Invalid("no video track"))?.clone();
        let Codec::Hevc { config } = &track.codec else { return Err(Error::Unsupported("not an HEVC track")) };
        let cfg = nal::hvcc(config)?;
        let mut d = Self::params_from(&cfg)?;
        let first = d.0.iter().flatten().next().ok_or(Error::Invalid("no SPS in hvcC"))?.clone();
        if first.chroma_format_idc != 1 {
            return Err(Error::Unsupported("HEVC other than 4:2:0"));
        }
        let (profile, format) = hevc_profile(first.bit_depth_luma)?;
        let gpu = Gpu::open(&profile, format)?;
        let (w, h) = first.display_size();
        let info = StreamInfo { width: w, height: h, bit_depth: first.bit_depth_luma, frame_count: track.samples.len(), frame_rate: track.frame_rate(), timescale: track.timescale, rotation: track.rotation, display: display_size(w, h, track.par, (first.vui.sar.0 as u32, first.vui.sar.1 as u32)), vui: first.vui.clone() };
        Ok(Self {
            file,
            len_size: cfg.len_size,
            track,
            sps: std::mem::take(&mut d.0),
            pps: std::mem::take(&mut d.1),
            gpu,
            profile,
            format,
            active: None,
            dpb: Dpb::new(0),
            next_sample: 0,
            pending: None,
            flushed: false,
            feedback: 0,
            info,
            buf: Vec::new(),
        })
    }

    fn params_from(cfg: &nal::Config) -> Result<(Vec<Option<Sps>>, Vec<Option<Pps>>)> {
        let mut sps = vec![None; 16];
        let mut pps = vec![None; 64];
        for u in &cfg.units {
            Self::param_set(&mut sps, &mut pps, u)?;
        }
        Ok((sps, pps))
    }

    fn param_set(sps: &mut [Option<Sps>], pps: &mut [Option<Pps>], unit: &[u8]) -> Result<()> {
        let Some(t) = hevc::nal_unit_type(unit) else { return Ok(()) };
        if t != nal_type::SPS && t != nal_type::PPS {
            return Ok(());
        }
        let rbsp = nal::unescape(unit);
        let mut b = BitReader::new(&rbsp);
        let h = hevc::nal_header(&mut b)?;
        if h.nuh_layer_id != 0 {
            return Ok(());
        }
        if t == nal_type::SPS {
            let s = hevc::sps(&mut b)?;
            let id = s.id as usize;
            sps[id] = Some(s);
        } else {
            let p = hevc::pps(&mut b)?;
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
    /// output is dropped; leading RASL pictures of a CRA picture are skipped like at the start
    /// of a stream. The next [`HevcDecoder::next_frame`] returns pictures from there on.
    pub fn seek_to_sample(&mut self, i: usize) {
        let pool = self.active.as_ref().map_or(0, |a| a.hw.surfaces());
        self.dpb = Dpb::new(pool);
        self.next_sample = i.min(self.track.samples.len());
        self.pending = None;
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
                return Ok(Some(Frame { index: f.surface as u32, texture: a.hw.texture.clone(), poc: f.poc, pts: f.pts, width: w, height: h, crop: a.sps.crop, bit_depth: a.sps.bit_depth_luma }));
            }
            if let Some(mut p) = self.pending.take() {
                if p.started.is_none() {
                    let pps = self.pps[p.sh.pps_id as usize].clone().ok_or(Error::Invalid("missing PPS"))?;
                    let sps = self.sps[pps.sps_id as usize].clone().ok_or(Error::Invalid("missing SPS"))?;
                    if !self.matches(&sps) {
                        if self.active.is_some() {
                            self.dpb.flush();
                            if self.dpb.has_output() {
                                self.pending = Some(p);
                                continue;
                            }
                        }
                        self.init(&sps)?;
                    }
                    match self.dpb.start_picture(p.kind, &p.sh, &sps)? {
                        None => continue,
                        Some(s) => {
                            p.started = Some(s);
                            self.pending = Some(p);
                            continue;
                        }
                    }
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
        let coded = (a.sps.width, a.sps.height);
        a.hw.read_back(&self.gpu, f.index as usize, coded, f.crop)
    }

    fn matches(&self, sps: &Sps) -> bool {
        self.active.as_ref().is_some_and(|a| {
            a.sps.width == sps.width
                && a.sps.height == sps.height
                && a.sps.bit_depth_luma == sps.bit_depth_luma
                && a.sps.chroma_format_idc == sps.chroma_format_idc
                && a.sps.max_dec_pic_buffering_minus1 >= sps.max_dec_pic_buffering_minus1
        })
    }

    fn init(&mut self, sps: &Sps) -> Result<()> {
        if sps.chroma_format_idc != 1 || sps.bit_depth_luma != sps.bit_depth_chroma {
            return Err(Error::Unsupported("HEVC other than 4:2:0 with equal luma and chroma depth"));
        }
        let (profile, format) = hevc_profile(sps.bit_depth_luma)?;
        if profile != self.profile || format != self.format {
            return Err(Error::Unsupported("bit depth change inside a stream"));
        }
        // the buffer holds max_dec_pic_buffering pictures, one is out with the caller, one spare
        let pool = (sps.max_dec_pic_buffering_minus1 as usize + 1 + 3).min(16);
        let hw = HwDecoder::new(&self.gpu, &profile, format, sps.width, sps.height, pool)?;
        log::debug!("HEVC decoder on {}: {}x{}, {} surfaces, ConfigBitstreamRaw {}", self.gpu.adapter_name, sps.width, sps.height, pool, hw.config_bitstream_raw);
        self.dpb = Dpb::new(pool);
        self.active = Some(Active { hw, sps: sps.clone() });
        Ok(())
    }

    /// Reads the next sample: parameter sets are taken up, the slices become the pending picture.
    fn read_sample(&mut self) -> Result<()> {
        let i = self.next_sample;
        self.next_sample += 1;
        let pts = self.track.samples[i].pts;
        self.track.read_sample(&mut self.file, i, &mut self.buf)?;
        let buf = std::mem::take(&mut self.buf);
        let r = self.parse_sample(&buf, pts);
        self.buf = buf;
        r
    }

    fn parse_sample(&mut self, sample: &[u8], pts: i64) -> Result<()> {
        for u in nal::units(sample, self.len_size) {
            let u = u?;
            if u.len() < 3 {
                continue;
            }
            let Some(t) = hevc::nal_unit_type(u) else { continue };
            match t {
                nal_type::SPS | nal_type::PPS => Self::param_set(&mut self.sps, &mut self.pps, u)?,
                nal_type::EOS | nal_type::EOB => self.dpb.end_of_sequence(),
                0..=9 | 16..=21 => {
                    let head = nal::unescape(&u[..u.len().min(256)]);
                    let mut b = BitReader::new(&head);
                    let nh = hevc::nal_header(&mut b)?;
                    if nh.nuh_layer_id != 0 {
                        continue;
                    }
                    let sh = {
                        let (pps, sps) = (&self.pps, &self.sps);
                        slice_header(
                            &mut b,
                            &nh,
                            |id| pps.get(id as usize).and_then(|p| p.as_ref()),
                            |id| sps.get(id as usize).and_then(|s| s.as_ref()),
                        )?
                    };
                    if sh.first_slice_segment_in_pic {
                        if self.pending.is_some() {
                            return Err(Error::Invalid("a picture was not decoded before the next one began"));
                        }
                        self.pending = Some(Pending { kind: PicKind::new(&nh), sh, pts, slices: vec![u.to_vec()], started: None });
                    } else if let Some(p) = &mut self.pending {
                        p.slices.push(u.to_vec());
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn decode(&mut self, p: Pending) -> Result<()> {
        let started = p.started.as_ref().ok_or(Error::Invalid("picture not started"))?;
        let pps = self.pps[p.sh.pps_id as usize].as_ref().ok_or(Error::Invalid("missing PPS"))?;
        let sps = self.sps[pps.sps_id as usize].as_ref().ok_or(Error::Invalid("missing SPS"))?;
        let surface = self.dpb.alloc().ok_or(Error::Invalid("no free surface"))?;
        self.feedback = self.feedback.wrapping_add(1).max(1);
        let refs = self.dpb.ref_pics();
        let pp = dxva::pic_params(&PicInfo { sps, pps, kind: p.kind, sh: &p.sh, poc: started.poc, surface, refs: &refs, sets: &started.sets, feedback: self.feedback });
        let qm = dxva::qmatrix(sps, pps);

        let mut bitstream = Vec::with_capacity(p.slices.iter().map(|s| s.len() + 3).sum());
        let mut slices = Vec::with_capacity(p.slices.len());
        for s in &p.slices {
            slices.push(SliceShort { bs_nal_unit_data_location: bitstream.len() as u32, slice_bytes_in_buffer: s.len() as u32 + 3, bad_slice_chopping: 0 });
            bitstream.extend_from_slice(&[0, 0, 1]);
            bitstream.extend_from_slice(s);
        }
        let a = self.active.as_mut().ok_or(Error::Invalid("no decoder"))?;
        a.hw.decode(&self.gpu, surface, dxva::bytes_of(&pp), qm.as_ref().map(dxva::bytes_of), &mut slices, &bitstream)?;
        // RASL pictures that reach decoding belong to an IRAP picture that decodes normally
        let sps = sps.clone();
        self.dpb.finish_picture(surface, started.poc, p.pts, p.sh.pic_output, &sps);
        Ok(())
    }
}
