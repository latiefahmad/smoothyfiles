//! `Av1Decoder`: an MP4 / MOV / Matroska / WebM file with AV1 in, decoded pictures out on the GPU.
//! The same shape as [`crate::Vp9Decoder`].
//!
//! AV1 shows frames in decode order too (hidden frames are shown later with
//! `show_existing_frame`), so the host only keeps the eight reference slots (which surface holds
//! which picture) and, per surface, how many slots and queued outputs still use it. A frame is
//! decoded when its last tile group arrives: all its tile data goes to the video engine in one
//! submission, with one `DXVA_Tile_AV1` per tile.

use std::collections::VecDeque;
use std::fs::File;
use std::path::Path;

use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::core::GUID;

use crate::av1::{self, Header, Parser, Sequence};
use crate::d3d11::{Gpu, HwDecoder, Planes, av1_profile};
use crate::decoder::{Frame, StreamInfo};
use crate::dxva::{self};
use crate::dxva_av1::{self, PicInfo, Tile};
use crate::mp4::{Codec, Track};
use crate::{Error, Result};

/// Surfaces: 8 reference slots, the picture being decoded, the one the caller holds, spares.
const POOL: usize = 12;

struct Out {
    surface: usize,
    pts: i64,
    width: u32,
    height: u32,
}

/// A frame whose header has been read and whose tile groups are still arriving.
struct Pending {
    header: Header,
    data: Vec<u8>,
    tiles: Vec<Tile>,
}

pub struct Av1Decoder {
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
    pending: Option<Pending>,
    next_sample: usize,
    feedback: u32,
    counter: i32,
    info: StreamInfo,
    buf: Vec<u8>,
}

fn vui_of(track: &Track, s: &Sequence) -> crate::hevc::Vui {
    let mut v = crate::hevc::Vui::default();
    v.full_range = s.full_range;
    v.colour_primaries = s.color_primaries;
    v.transfer_characteristics = s.transfer_characteristics;
    v.matrix_coefficients = s.matrix_coefficients;
    if let Some(c) = track.colour {
        v.full_range = c.full_range.unwrap_or(s.full_range);
        v.colour_primaries = c.primaries;
        v.transfer_characteristics = c.transfer;
        v.matrix_coefficients = c.matrix;
    }
    v
}

fn check_supported(s: &Sequence) -> Result<()> {
    if s.profile != 0 {
        return Err(Error::Unsupported("AV1 other than the main profile (4:4:4 and 4:2:2)"));
    }
    if s.mono_chrome {
        return Err(Error::Unsupported("monochrome AV1"));
    }
    if s.bit_depth != 8 && s.bit_depth != 10 {
        return Err(Error::Unsupported("AV1 bit depth other than 8 and 10"));
    }
    Ok(())
}

/// The sequence header of a packet or of the `av1C` configuration OBUs.
fn find_sequence(data: &[u8]) -> Option<Sequence> {
    let obus = av1::split_obus(data).ok()?;
    obus.iter().find(|o| o.kind == av1::OBU_SEQUENCE_HEADER).and_then(|o| av1::parse_sequence(o.payload).ok())
}

impl Av1Decoder {
    pub fn open(path: &Path) -> Result<Self> {
        let mut file = File::open(path)?;
        let movie = crate::container::read(&mut file)?;
        let track = movie.video().ok_or(Error::Invalid("no video track"))?.clone();
        let Codec::Av1 { config } = &track.codec else {
            return Err(Error::Unsupported("not an AV1 track"));
        };
        let mut buf = Vec::new();
        track.read_sample(&mut file, 0, &mut buf)?;
        let seq = config.get(4..).and_then(find_sequence).or_else(|| find_sequence(&buf)).ok_or(Error::Invalid("no AV1 sequence header"))?;
        check_supported(&seq)?;
        let (profile, format) = av1_profile(seq.bit_depth)?;
        let gpu = Gpu::open(&profile, format)?;
        let info = StreamInfo {
            width: seq.max_frame_width,
            height: seq.max_frame_height,
            bit_depth: seq.bit_depth,
            frame_count: track.samples.len(),
            frame_rate: track.frame_rate(),
            timescale: track.timescale,
            rotation: 0,
            display: crate::decoder::display_size(seq.max_frame_width, seq.max_frame_height, track.par, (0, 0)),
            vui: vui_of(&track, &seq),
        };
        let mut parser = Parser::new();
        parser.seq = Some(seq.clone());
        Ok(Self {
            file,
            track,
            gpu,
            profile,
            format,
            bit_depth: seq.bit_depth,
            hw: None,
            hw_size: (0, 0),
            parser,
            slots: [None; 8],
            users: [0; POOL],
            held: None,
            ready: VecDeque::new(),
            pending: None,
            next_sample: 0,
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

    /// Continues decoding at sample `i`, which must start with a key frame. All references and
    /// queued pictures are dropped.
    pub fn seek_to_sample(&mut self, i: usize) {
        self.parser.reset();
        self.slots = [None; 8];
        self.users = [0; POOL];
        self.held = None;
        self.ready.clear();
        self.pending = None;
        self.next_sample = i.min(self.track.samples.len());
    }

    pub fn adapter(&self) -> &str {
        &self.gpu.adapter_name
    }

    /// ConfigBitstreamRaw of the decoder configuration in use, once decoding has started.
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
                self.held = Some(o.surface);
                let hw = self.hw.as_ref().ok_or(Error::Invalid("frame without decoder"))?;
                self.counter += 1;
                return Ok(Some(Frame { index: o.surface as u32, texture: hw.texture.clone(), poc: self.counter - 1, pts: o.pts, width: o.width, height: o.height, crop: (0, 0, 0, 0), bit_depth: self.bit_depth }));
            }
            if self.next_sample < self.track.samples.len() {
                let i = self.next_sample;
                self.next_sample += 1;
                let pts = self.track.samples[i].pts;
                let mut buf = std::mem::take(&mut self.buf);
                let r = self.track.read_sample(&mut self.file, i, &mut buf).map_err(Error::from).and_then(|_| self.unit(&buf, pts));
                self.buf = buf;
                r?;
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
        log::debug!("AV1 decoder on {}: {w}x{h}, {POOL} surfaces, ConfigBitstreamRaw {}", self.gpu.adapter_name, hw.config_bitstream_raw);
        self.hw = Some(hw);
        self.hw_size = (w, h);
        self.slots = [None; 8];
        self.users = [0; POOL];
        Ok(())
    }

    /// One temporal unit.
    fn unit(&mut self, data: &[u8], pts: i64) -> Result<()> {
        for obu in av1::split_obus(data)? {
            match obu.kind {
                av1::OBU_SEQUENCE_HEADER => {
                    let s = av1::parse_sequence(obu.payload)?;
                    check_supported(&s)?;
                    if s.bit_depth != self.bit_depth {
                        return Err(Error::Unsupported("bit depth change inside an AV1 stream"));
                    }
                    self.parser.seq = Some(s);
                }
                av1::OBU_TEMPORAL_DELIMITER => self.pending = None,
                av1::OBU_FRAME_HEADER | av1::OBU_FRAME | av1::OBU_REDUNDANT_FRAME_HEADER | av1::OBU_TILE_GROUP => {
                    let idc = self.parser.seq.as_ref().map_or(0, |s| s.operating_point_idc);
                    if let (Some((t, s)), true) = (obu.layer, idc != 0) {
                        if (idc >> t) & 1 == 0 || (idc >> (s + 8)) & 1 == 0 {
                            continue;
                        }
                    }
                    let (t, s) = obu.layer.unwrap_or((0, 0));
                    self.parser.temporal_id = t;
                    self.parser.spatial_id = s;
                    match obu.kind {
                        av1::OBU_TILE_GROUP => self.tile_group(obu.payload, pts)?,
                        av1::OBU_REDUNDANT_FRAME_HEADER => {}
                        kind => {
                            if self.pending.is_some() {
                                continue; // a copy of the header already read
                            }
                            let h = self.parser.parse_frame_header(obu.payload)?;
                            if h.show_existing_frame {
                                self.show_existing(&h, pts)?;
                                continue;
                            }
                            let at = h.header_bytes;
                            self.pending = Some(Pending { header: h, data: Vec::new(), tiles: Vec::new() });
                            if kind == av1::OBU_FRAME {
                                let body = obu.payload.get(at..).ok_or(Error::Invalid("AV1 frame OBU cut short"))?;
                                self.tile_group(body, pts)?;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn tile_group(&mut self, body: &[u8], pts: i64) -> Result<()> {
        let Some(p) = self.pending.as_mut() else { return Err(Error::Invalid("AV1 tile group without a frame header")) };
        let (_start, end, data_at, tiles) = av1::parse_tile_group(&p.header, body)?;
        let at = p.data.len();
        p.tiles.extend(dxva_av1::make_tiles(&tiles, at));
        p.data.extend_from_slice(&body[data_at..]);
        if end == p.header.tile_cols * p.header.tile_rows - 1 {
            let p = self.pending.take().unwrap();
            self.decode(p, pts)?;
        }
        Ok(())
    }

    fn show_existing(&mut self, h: &Header, pts: i64) -> Result<()> {
        let s = self.slots[h.frame_to_show as usize].ok_or(Error::Invalid("AV1 show_existing_frame of an empty slot"))?;
        self.users[s] += 1;
        self.ready.push_back(Out { surface: s, pts, width: h.upscaled_width, height: h.frame_height });
        if h.frame_type == av1::KEY_FRAME {
            // the shown key frame refreshes every slot
            for i in 0..8 {
                if let Some(old) = self.slots[i].replace(s) {
                    self.users[old] = self.users[old].saturating_sub(1);
                }
                self.users[s] += 1;
            }
        }
        self.parser.finish(h);
        Ok(())
    }

    fn decode(&mut self, p: Pending, pts: i64) -> Result<()> {
        let h = p.header;
        let seq = self.parser.seq.clone().ok_or(Error::Invalid("AV1 frame before the sequence header"))?;
        let fits = h.upscaled_width <= self.hw_size.0 && h.frame_height <= self.hw_size.1;
        if self.hw.is_none() || (h.frame_type == av1::KEY_FRAME && !fits) {
            if self.hw.is_some() && !self.ready.is_empty() {
                return Err(Error::Invalid("picture queued across a size change"));
            }
            self.init(seq.max_frame_width.max(h.upscaled_width), seq.max_frame_height.max(h.frame_height))?;
        } else if !fits {
            return Err(Error::Unsupported("AV1 frame larger than the decoder's surfaces"));
        }
        if !h.frame_is_intra {
            for i in 0..7 {
                if self.slots[h.ref_frame_idx[i] as usize].is_none() {
                    return Err(Error::Invalid("AV1 frame refers to an empty slot"));
                }
            }
        }
        let surface = self.alloc()?;
        // a non-zero feedback number made the driver crash on the 25th frame (nothing here asks
        // for status reports)
        self.feedback = 0;
        let mut dims = [(0u32, 0u32); 8];
        for i in 0..8 {
            // the size a reference was coded at (with superres: before upscaling), which is what the
            // driver scales its prediction from
            dims[i] = (self.parser.slots[i].frame_width, self.parser.slots[i].frame_height);
        }
        let pp = dxva_av1::pic_params(&PicInfo { seq: &seq, header: &h, surface, slots: self.slots, slot_dims: dims, feedback: self.feedback });
        let hw = self.hw.as_mut().ok_or(Error::Invalid("no decoder"))?;
        hw.decode_controls(&self.gpu, surface, dxva::bytes_of(&pp), None, dxva_av1::tile_bytes(&p.tiles), &p.data)?;

        for i in 0..8 {
            if h.refresh_frame_flags >> i & 1 != 0 {
                if let Some(old) = self.slots[i].replace(surface) {
                    self.users[old] = self.users[old].saturating_sub(1);
                }
                self.users[surface] += 1;
            }
        }
        self.parser.finish(&h);
        if h.show_frame {
            self.users[surface] += 1;
            self.ready.push_back(Out { surface, pts, width: h.upscaled_width, height: h.frame_height });
        }
        Ok(())
    }
}
