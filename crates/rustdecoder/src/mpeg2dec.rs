//! `Mpeg2Decoder`: MPEG-1 / MPEG-2 video (program stream, transport stream, MXF, raw `.m2v`)
//! in, decoded pictures out on the GPU's video engine, in display order.
//!
//! We parse the headers and cut the slices; the video engine does the macroblock layer
//! (DXVA 1.0 `MPEG2_VLD`: picture parameters, the four quantiser matrices, one `DXVA_SliceInfo`
//! per slice, the slices with their start codes in the bitstream buffer).
//!
//! Reordering: B pictures go out right after they are decoded; an I or P picture waits until
//! the next I or P picture is decoded (it is the backward reference in between). A picture
//! that is a reference stays on its surface until it is replaced as one and has been shown.
//!
//! Not done: field pictures (two fields into one surface) and 4:2:2 (no decoder profile for it
//! exists in D3D11); both are refused with a message.

use std::collections::VecDeque;
use std::fs::File;
use std::path::Path;

use windows::Win32::Graphics::Direct3D11::{D3D11_DECODER_PROFILE_MPEG2_VLD, D3D11_DECODER_PROFILE_MPEG2and1_VLD};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;
use windows::core::GUID;

use crate::d3d11::{Gpu, HwDecoder, Planes};
use crate::decoder::{Frame, StreamInfo};
use crate::dxva::{self};
use crate::dxva_mpeg2::{self, PicInfo};
use crate::mp4::Track;
use crate::mpeg2::{self, Matrices, PicType, Picture, Sequence};
use crate::{Error, Result};

/// Surfaces: two references, the picture being decoded, the one waiting to be shown, the one
/// the caller holds, spares.
const POOL: usize = 8;

struct Out {
    surface: usize,
    pts: i64,
}

pub struct Mpeg2Decoder {
    file: File,
    track: Track,
    gpu: Gpu,
    hw: Option<HwDecoder>,
    hw_key: (u32, u32),
    seq: Option<Sequence>,
    matrices: Matrices,
    closed_gop: bool,
    /// The two reference pictures: older, newer (surface, pts).
    older: Option<(usize, i64)>,
    newer: Option<(usize, i64)>,
    users: [u32; POOL],
    held: Option<usize>,
    ready: VecDeque<Out>,
    /// Pictures of the sample being worked through.
    units: VecDeque<(Vec<u8>, i64)>,
    next_sample: usize,
    flushed: bool,
    counter: i32,
    info: StreamInfo,
    buf: Vec<u8>,
}

fn profiles(mpeg2: bool) -> [GUID; 2] {
    if mpeg2 { [D3D11_DECODER_PROFILE_MPEG2_VLD, D3D11_DECODER_PROFILE_MPEG2and1_VLD] } else { [D3D11_DECODER_PROFILE_MPEG2and1_VLD, D3D11_DECODER_PROFILE_MPEG2_VLD] }
}

/// The sequence header (with its extension and matrices) in a block of stream bytes.
fn find_sequence(d: &[u8]) -> Option<Sequence> {
    let codes: Vec<(usize, u8)> = mpeg2::start_codes(d).collect();
    for (n, &(p, c)) in codes.iter().enumerate() {
        if c == mpeg2::SEQUENCE_HEADER {
            let end = codes.get(n + 1).map_or(d.len(), |x| x.0);
            let ext = codes.get(n + 1).filter(|x| x.1 == mpeg2::EXTENSION).map(|x| {
                let e = codes.get(n + 2).map_or(d.len(), |y| y.0);
                &d[x.0 + 4..e]
            });
            return mpeg2::parse_sequence(&d[p + 3..end], ext).ok();
        }
    }
    None
}

impl Mpeg2Decoder {
    pub fn open(path: &Path) -> Result<Self> {
        let mut file = File::open(path)?;
        let movie = crate::container::read(&mut file)?;
        let track = movie.video().ok_or(Error::Invalid("no video track"))?.clone();
        if &track.fourcc != b"mpg2" && &track.fourcc != b"mpg1" {
            return Err(Error::Unsupported("not an MPEG-1 / MPEG-2 video track"));
        }
        if track.samples.is_empty() {
            return Err(Error::Unsupported("MPEG video track without samples"));
        }
        // the first sequence header: in one of the first samples
        let mut seq = None;
        let mut buf = Vec::new();
        for i in 0..track.samples.len().min(30) {
            track.read_sample(&mut file, i, &mut buf)?;
            if let Some(s) = find_sequence(&buf) {
                seq = Some(s);
                break;
            }
        }
        let seq = seq.ok_or(Error::Invalid("no MPEG sequence header in the first samples"))?;
        check_supported(&seq)?;
        let mut last = Error::Gpu("no decoder profile".into());
        let mut gpu = None;
        for p in profiles(seq.mpeg2) {
            match Gpu::open(&p, DXGI_FORMAT_NV12) {
                Ok(g) => {
                    gpu = Some((g, p));
                    break;
                }
                Err(e) => last = e,
            }
        }
        let (gpu, _) = gpu.ok_or(last)?;
        let dar = match (seq.mpeg2, seq.aspect_code) {
            (true, 2) => (4, 3),
            (true, 3) => (16, 9),
            (true, 4) => (221, 100),
            _ => (0, 0),
        };
        let sar = if dar.0 == 0 { (0, 0) } else { (dar.0 * seq.height, dar.1 * seq.width) };
        let mut vui = crate::hevc::Vui::default();
        vui.matrix_coefficients = if seq.height >= 720 { 1 } else { 6 };
        let info = StreamInfo {
            width: seq.width,
            height: seq.height,
            bit_depth: 8,
            frame_count: track.samples.len(),
            frame_rate: if seq.frame_rate() > 0.0 { seq.frame_rate() } else { track.frame_rate() },
            timescale: track.timescale,
            rotation: 0,
            display: crate::decoder::display_size(seq.width, seq.height, track.par, sar),
            vui,
        };
        Ok(Self {
            file,
            track,
            gpu,
            hw: None,
            hw_key: (0, 0),
            matrices: seq.matrices.clone(),
            seq: Some(seq),
            closed_gop: false,
            older: None,
            newer: None,
            users: [0; POOL],
            held: None,
            ready: VecDeque::new(),
            units: VecDeque::new(),
            next_sample: 0,
            flushed: false,
            counter: 0,
            info,
            buf: Vec::new(),
        })
    }

    pub fn info(&self) -> &StreamInfo {
        &self.info
    }

    pub fn gpu(&self) -> &Gpu {
        &self.gpu
    }

    pub fn track(&self) -> &Track {
        &self.track
    }

    pub fn adapter(&self) -> &str {
        &self.gpu.adapter_name
    }

    pub fn config_bitstream_raw(&self) -> Option<u32> {
        self.hw.as_ref().map(|h| h.config_bitstream_raw)
    }

    pub fn wait_idle(&self) -> Result<()> {
        self.gpu.wait_idle()
    }

    pub fn read_back(&mut self, f: &Frame) -> Result<Planes> {
        let hw = self.hw.as_mut().ok_or(Error::Invalid("no decoder"))?;
        hw.read_back(&self.gpu, f.index as usize, (f.width, f.height), f.crop)
    }

    /// Continues decoding at sample `i`, which should start a GOP with an I picture. References
    /// and queued pictures are dropped.
    pub fn seek_to_sample(&mut self, i: usize) {
        self.older = None;
        self.newer = None;
        self.users = [0; POOL];
        self.held = None;
        self.ready.clear();
        self.units.clear();
        self.flushed = false;
        self.next_sample = i.min(self.track.samples.len());
    }

    fn release(&mut self, surface: usize) {
        self.users[surface] = self.users[surface].saturating_sub(1);
    }

    /// The next picture in display order; `None` at the end. A frame stays valid until the next
    /// call.
    pub fn next_frame(&mut self) -> Result<Option<Frame>> {
        if let Some(s) = self.held.take() {
            self.release(s);
        }
        loop {
            if let Some(o) = self.ready.pop_front() {
                self.held = Some(o.surface);
                let hw = self.hw.as_ref().ok_or(Error::Invalid("frame without decoder"))?;
                self.counter += 1;
                let (w, h) = self.seq.as_ref().map_or((0, 0), |s| (s.width, s.height));
                return Ok(Some(Frame { index: o.surface as u32, texture: hw.texture.clone(), poc: self.counter - 1, pts: o.pts, width: w, height: h, crop: (0, 0, 0, 0), bit_depth: 8 }));
            }
            if let Some((data, pts)) = self.units.pop_front() {
                self.decode(&data, pts)?;
                continue;
            }
            if self.next_sample < self.track.samples.len() {
                let i = self.next_sample;
                self.next_sample += 1;
                let pts = self.track.samples[i].pts;
                self.track.read_sample(&mut self.file, i, &mut self.buf)?;
                for (a, b) in mpeg2::split_units(&self.buf) {
                    self.units.push_back((self.buf[a..b].to_vec(), pts));
                }
                continue;
            }
            if !self.flushed {
                self.flushed = true;
                if let Some((s, pts)) = self.newer {
                    self.users[s] += 1;
                    self.ready.push_back(Out { surface: s, pts });
                    continue;
                }
            }
            return Ok(None);
        }
    }

    fn alloc(&mut self) -> Result<usize> {
        let n = self.hw.as_ref().map_or(0, |h| h.surfaces()).min(POOL);
        (0..n).find(|&s| self.users[s] == 0).ok_or(Error::Invalid("no free surface"))
    }

    fn init(&mut self, seq: &Sequence) -> Result<()> {
        let mut last = Error::Gpu("no decoder profile".into());
        for p in profiles(seq.mpeg2) {
            match HwDecoder::with_alignment(&self.gpu, &p, DXGI_FORMAT_NV12, seq.width, seq.height, POOL, 32, 1) {
                Ok(hw) => {
                    log::debug!("MPEG-2 decoder on {}: {}x{}, ConfigBitstreamRaw {}", self.gpu.adapter_name, seq.width, seq.height, hw.config_bitstream_raw);
                    self.hw = Some(hw);
                    self.hw_key = (seq.width, seq.height);
                    self.users = [0; POOL];
                    self.older = None;
                    self.newer = None;
                    return Ok(());
                }
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// One access unit: optional sequence / GOP headers, then a picture.
    fn decode(&mut self, d: &[u8], pts: i64) -> Result<()> {
        let codes: Vec<(usize, u8)> = mpeg2::start_codes(d).collect();
        let mut pic: Option<(Picture, usize)> = None;
        for (n, &(p, c)) in codes.iter().enumerate() {
            let end = codes.get(n + 1).map_or(d.len(), |x| x.0);
            let body = &d[(p + 4).min(end)..end];
            match c {
                mpeg2::SEQUENCE_HEADER => {
                    let ext = codes.get(n + 1).filter(|x| x.1 == mpeg2::EXTENSION).map(|x| {
                        let e = codes.get(n + 2).map_or(d.len(), |y| y.0);
                        &d[x.0 + 4..e]
                    });
                    let s = mpeg2::parse_sequence(&d[p + 3..end], ext)?;
                    check_supported(&s)?;
                    self.matrices = s.matrices.clone();
                    let changed = self.hw.is_none() || self.hw_key != (s.width, s.height);
                    if changed {
                        if self.hw.is_some() {
                            // the newest reference is shown before the new size starts
                            self.flush_newer();
                        }
                        self.seq = Some(s.clone());
                        self.init(&s)?;
                    } else {
                        self.seq = Some(s);
                    }
                }
                mpeg2::GROUP_START => self.closed_gop = mpeg2::gop_closed(body),
                mpeg2::EXTENSION => match body.first().map(|b| b >> 4) {
                    Some(3) => mpeg2::parse_quant_ext(body, &mut self.matrices)?,
                    Some(8) => {
                        if let Some((pc, _)) = pic.as_mut() {
                            mpeg2::apply_coding_ext(pc, body)?;
                        }
                    }
                    _ => {}
                },
                mpeg2::PICTURE_START => pic = Some((mpeg2::parse_picture(body)?, p)),
                _ => {}
            }
        }
        let Some((pic, at)) = pic else { return Ok(()) };
        self.decode_picture(&pic, &d[at..], pts)
    }

    /// Queues the newest reference for output (before a size change).
    fn flush_newer(&mut self) {
        // the surfaces go with the old decoder: what is queued is lost
        self.ready.clear();
        self.newer = None;
        self.older = None;
    }

    fn decode_picture(&mut self, pic: &Picture, d: &[u8], pts: i64) -> Result<()> {
        let seq = self.seq.clone().ok_or(Error::Invalid("MPEG picture before a sequence header"))?;
        if pic.structure != 3 {
            return Err(Error::Unsupported("MPEG-2 field pictures (two fields into one surface) are not done yet"));
        }
        // references; a picture whose references are missing (after a seek) is dropped
        let (forward, backward) = match pic.coding_type {
            PicType::I => (None, None),
            PicType::P => match self.newer {
                Some(n) => (Some(n.0), None),
                None => return Ok(()),
            },
            PicType::B => match (self.older, self.newer) {
                (Some(o), Some(n)) => (Some(o.0), Some(n.0)),
                (None, Some(n)) if self.closed_gop => (Some(n.0), Some(n.0)),
                _ => return Ok(()),
            },
        };
        let slices = mpeg2::slices(d, seq.height > 2800);
        if slices.is_empty() {
            return Err(Error::Invalid("MPEG picture without slices"));
        }
        let surface = self.alloc()?;
        let mut bitstream = Vec::with_capacity(d.len());
        let mut locations = Vec::with_capacity(slices.len());
        for s in &slices {
            locations.push(bitstream.len() as u32);
            bitstream.extend_from_slice(&d[s.start..s.end]);
        }
        let pp = dxva_mpeg2::pic_params(&PicInfo { seq: &seq, pic, surface, forward, backward, second_field: false });
        let qm = dxva_mpeg2::qmatrix(&self.matrices);
        let infos = dxva_mpeg2::slice_infos(&seq, &slices, &locations);
        let hw = self.hw.as_mut().ok_or(Error::Invalid("no decoder"))?;
        hw.decode_controls(&self.gpu, surface, dxva::bytes_of(&pp), Some(dxva::bytes_of(&qm)), dxva_mpeg2::slice_bytes(&infos), &bitstream)?;

        if pic.coding_type == PicType::B {
            self.users[surface] += 1;
            self.ready.push_back(Out { surface, pts });
        } else {
            // the previous newest reference is next in display order
            if let Some((s, p)) = self.newer {
                self.users[s] += 1;
                self.ready.push_back(Out { surface: s, pts: p });
            }
            if let Some((s, _)) = self.older.take() {
                self.release(s);
            }
            self.older = self.newer;
            self.newer = Some((surface, pts));
            self.users[surface] += 1;
        }
        Ok(())
    }
}

fn check_supported(s: &Sequence) -> Result<()> {
    if !s.mpeg2 {
        // MPEG2and1_VLD on the NVIDIA driver tested here decodes the first macroblocks of an
        // MPEG-1 picture and then loses the bitstream (its escape codes and dequantiser differ
        // from MPEG-2 and nothing in the DXVA structures says "MPEG-1"): refused, not shown wrong
        return Err(Error::Unsupported("MPEG-1 video: the video engine's MPEG-1 mode did not decode correctly on the GPU tested"));
    }
    if s.chroma_format != 1 {
        return Err(Error::Unsupported("MPEG-2 4:2:2 / 4:4:4 (D3D11 has no decoder profile for it)"));
    }
    Ok(())
}
