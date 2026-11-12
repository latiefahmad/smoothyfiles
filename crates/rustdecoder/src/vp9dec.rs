//! `Vp9Decoder`: a Matroska / WebM file with VP9 in, decoded pictures out on the GPU. The same
//! shape as [`crate::HevcDecoder`] and [`crate::H264Decoder`].
//!
//! VP9 has no picture reordering: a shown frame goes out in decode order. What the host keeps is
//! the eight reference slots (which surface holds which picture) and, per surface, how many
//! slots and queued outputs still use it; a surface with no users is free for the next frame.

use std::collections::VecDeque;
use std::fs::File;
use std::path::Path;

use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::core::GUID;


use crate::d3d11::{Gpu, HwDecoder, Planes, vp9_profile};
use crate::decoder::{Frame, StreamInfo};
use crate::dxva::{self, SliceShort};
use crate::dxva_vp9::{self, PicInfo};
use crate::mp4::{Codec, Track};
use crate::vp9::{self, Header, Parser};
use crate::{Error, Result};

/// Surfaces: 8 reference slots, the picture being decoded, the one the caller holds, spares.
const POOL: usize = 12;

struct Out {
    surface: usize,
    pts: i64,
    width: u32,
    height: u32,
}

pub struct Vp9Decoder {
    file: File,
    track: Track,
    gpu: Gpu,
    profile: GUID,
    format: DXGI_FORMAT,
    bit_depth: u8,
    hw: Option<HwDecoder>,
    /// Size the decoder and its surfaces were made for.
    hw_size: (u32, u32),
    parser: Parser,
    slots: [Option<usize>; 8],
    users: [u32; POOL],
    held: Option<usize>,
    ready: VecDeque<Out>,
    /// Frames of the packet being worked through, with its time.
    frames: VecDeque<(Vec<u8>, i64)>,
    next_sample: usize,
    /// Size and `show_frame` of the frame decoded last.
    last: Option<(u32, u32, bool)>,
    feedback: u32,
    counter: i32,
    info: StreamInfo,
    buf: Vec<u8>,
}

fn vui_of(track: &Track, h: &Header) -> crate::hevc::Vui {
    let mut v = crate::hevc::Vui::default();
    if let Some(c) = track.colour {
        v.full_range = c.full_range.unwrap_or(h.full_range);
        v.colour_primaries = c.primaries;
        v.transfer_characteristics = c.transfer;
        v.matrix_coefficients = c.matrix;
    } else {
        v.full_range = h.full_range;
        v.matrix_coefficients = match h.color_space {
            1 | 3 => 6,
            2 => 1,
            4 => 7,
            5 => 9,
            _ => 2,
        };
        v.colour_primaries = if h.color_space == 5 { 9 } else { 0 };
    }
    v
}

impl Vp9Decoder {
    pub fn open(path: &Path) -> Result<Self> {
        let mut file = File::open(path)?;
        let movie = crate::container::read(&mut file)?;
        let track = movie.video().ok_or(Error::Invalid("no video track"))?.clone();
        if !matches!(track.codec, Codec::Vp9 { .. }) {
            return Err(Error::Unsupported("not a VP9 track"));
        }
        // the first frame tells the profile and bit depth
        let mut buf = Vec::new();
        track.read_sample(&mut file, 0, &mut buf)?;
        let frames = vp9::split_superframe(&buf)?;
        let first = frames.first().ok_or(Error::Invalid("empty first VP9 packet"))?;
        let h = Parser::new().parse(first)?;
        if !h.key_frame {
            return Err(Error::Invalid("VP9 stream does not start with a key frame"));
        }
        check_supported(&h)?;
        let (profile, format) = vp9_profile(h.bit_depth)?;
        let gpu = Gpu::open(&profile, format)?;
        let info = StreamInfo {
            width: h.width,
            height: h.height,
            bit_depth: h.bit_depth,
            frame_count: track.samples.len(),
            frame_rate: track.frame_rate(),
            timescale: track.timescale,
            rotation: 0,
            display: crate::decoder::display_size(h.width, h.height, track.par, (0, 0)),
            vui: vui_of(&track, &h),
        };
        Ok(Self {
            file,
            track,
            gpu,
            profile,
            format,
            bit_depth: h.bit_depth,
            hw: None,
            hw_size: (0, 0),
            parser: Parser::new(),
            slots: [None; 8],
            users: [0; POOL],
            held: None,
            ready: VecDeque::new(),
            frames: VecDeque::new(),
            next_sample: 0,
            last: None,
            feedback: 0,
            counter: 0,
            info,
            buf: Vec::new(),
        })
    }

    pub fn info(&self) -> &StreamInfo {
        &self.info
    }

    /// The device the pictures live on.
    pub fn gpu(&self) -> &Gpu {
        &self.gpu
    }

    /// The video track with its sample table.
    pub fn track(&self) -> &Track {
        &self.track
    }

    /// Continues decoding at sample `i`, which must start with a key frame. All references
    /// and queued pictures are dropped.
    pub fn seek_to_sample(&mut self, i: usize) {
        self.parser.reset();
        self.slots = [None; 8];
        self.users = [0; POOL];
        self.held = None;
        self.ready.clear();
        self.frames.clear();
        self.last = None;
        self.next_sample = i.min(self.track.samples.len());
    }

    pub fn adapter(&self) -> &str {
        &self.gpu.adapter_name
    }

    /// ConfigBitstreamRaw of the decoder configuration in use (1: short slice format), once
    /// decoding has started.
    pub fn config_bitstream_raw(&self) -> Option<u32> {
        self.hw.as_ref().map(|h| h.config_bitstream_raw)
    }

    pub fn wait_idle(&self) -> Result<()> {
        self.gpu.wait_idle()
    }

    /// Copies a frame to the CPU (for tests): planes, one `u16` per sample.
    pub fn read_back(&mut self, f: &Frame) -> Result<Planes> {
        let hw = self.hw.as_mut().ok_or(Error::Invalid("no decoder"))?;
        hw.read_back(&self.gpu, f.index as usize, (f.width, f.height), f.crop)
    }

    fn release(&mut self, surface: usize) {
        self.users[surface] = self.users[surface].saturating_sub(1);
    }

    /// The next shown picture; `None` at the end of the stream. A frame stays valid until the
    /// next call.
    pub fn next_frame(&mut self) -> Result<Option<Frame>> {
        if let Some(s) = self.held.take() {
            self.release(s);
        }
        loop {
            if let Some(o) = self.ready.pop_front() {
                // the queue's claim on the surface is now the caller's, ended at the next call
                self.held = Some(o.surface);
                let hw = self.hw.as_ref().ok_or(Error::Invalid("frame without decoder"))?;
                self.counter += 1;
                return Ok(Some(Frame { index: o.surface as u32, texture: hw.texture.clone(), poc: self.counter - 1, pts: o.pts, width: o.width, height: o.height, crop: (0, 0, 0, 0), bit_depth: self.bit_depth }));
            }
            if let Some((data, pts)) = self.frames.pop_front() {
                self.decode(&data, pts)?;
                continue;
            }
            if self.next_sample < self.track.samples.len() {
                let i = self.next_sample;
                self.next_sample += 1;
                let pts = self.track.samples[i].pts;
                self.track.read_sample(&mut self.file, i, &mut self.buf)?;
                for f in vp9::split_superframe(&self.buf)? {
                    self.frames.push_back((f.to_vec(), pts));
                }
                continue;
            }
            return Ok(None);
        }
    }

    fn alloc(&mut self) -> Result<usize> {
        let n = self.hw.as_ref().map_or(0, |h| h.surfaces()).min(POOL);
        (0..n).find(|&s| self.users[s] == 0).ok_or(Error::Invalid("no free surface"))
    }

    fn init(&mut self, w: u32, h: u32) -> Result<()> {
        let hw = HwDecoder::with_alignment(&self.gpu, &self.profile, self.format, w, h, POOL, 64, 1)?;
        log::debug!("VP9 decoder on {}: {w}x{h}, {POOL} surfaces, ConfigBitstreamRaw {}", self.gpu.adapter_name, hw.config_bitstream_raw);
        self.hw = Some(hw);
        self.hw_size = (w, h);
        self.slots = [None; 8];
        self.users = [0; POOL];
        Ok(())
    }

    fn decode(&mut self, data: &[u8], pts: i64) -> Result<()> {
        let h = self.parser.parse(data)?;
        if let Some(idx) = h.show_existing {
            let s = self.slots[idx as usize].ok_or(Error::Invalid("VP9 show_existing_frame of an empty slot"))?;
            self.users[s] += 1;
            self.ready.push_back(Out { surface: s, pts, width: h.width, height: h.height });
            return Ok(());
        }
        check_supported(&h)?;
        if h.bit_depth != self.bit_depth {
            return Err(Error::Unsupported("bit depth change inside a VP9 stream"));
        }
        let fits = h.width <= self.hw_size.0 && h.height <= self.hw_size.1;
        if self.hw.is_none() || (h.key_frame && !fits) {
            if self.hw.is_some() && !self.ready.is_empty() {
                return Err(Error::Invalid("picture queued across a size change"));
            }
            self.init(h.width, h.height)?;
        } else if !fits {
            return Err(Error::Unsupported("VP9 inter frame larger than the first frame (the surfaces are sized by the key frame)"));
        }
        if !h.is_intra() {
            for i in 0..3 {
                if self.slots[h.ref_frame_idx[i] as usize].is_none() {
                    return Err(Error::Invalid("VP9 frame refers to an empty slot"));
                }
            }
        }
        let surface = self.alloc()?;
        self.feedback = self.feedback.wrapping_add(1).max(1);
        let use_prev = !h.error_resilient && self.last.is_some_and(|(w, hh, shown)| w == h.width && hh == h.height && shown);
        let pp = dxva_vp9::pic_params(&PicInfo { header: &h, surface, slots: self.slots, slot_dims: self.parser.ref_dims, use_prev_mvs: use_prev, feedback: self.feedback });
        let mut slices = [SliceShort { bs_nal_unit_data_location: 0, slice_bytes_in_buffer: data.len() as u32, bad_slice_chopping: 0 }];
        let hw = self.hw.as_mut().ok_or(Error::Invalid("no decoder"))?;
        hw.decode(&self.gpu, surface, dxva::bytes_of(&pp), None, &mut slices, data)?;

        // refresh the slots, queue the picture
        for i in 0..8 {
            if h.refresh_frame_flags >> i & 1 != 0 {
                if let Some(old) = self.slots[i].replace(surface) {
                    self.users[old] = self.users[old].saturating_sub(1);
                }
                self.users[surface] += 1;
            }
        }
        self.parser.refresh(h.refresh_frame_flags, h.width, h.height);
        self.last = Some((h.width, h.height, h.show_frame));
        if h.show_frame {
            self.users[surface] += 1;
            self.ready.push_back(Out { surface, pts, width: h.width, height: h.height });
        }
        Ok(())
    }
}

/// What the header says is wrong with the stream for this decoder, if anything.
fn check_supported(h: &Header) -> Result<()> {
    if !(h.subsampling_x && h.subsampling_y) {
        return Err(Error::Unsupported("VP9 other than 4:2:0 (profiles 1 and 3)"));
    }
    if h.bit_depth != 8 && h.bit_depth != 10 {
        return Err(Error::Unsupported("VP9 12-bit"));
    }
    Ok(())
}
