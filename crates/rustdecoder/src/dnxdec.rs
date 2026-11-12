//! `DnxVideo`: a MOV with DNxHD / DNxHR in, pictures out on the GPU. The same shape and the
//! same upload as [`crate::proresdec::DnxVideo`] (so `VideoDecoder`, `Player` and `Playback`
//! take it unchanged); the decoding itself is [`crate::dnx`], on the CPU with the macroblock
//! rows spread over a thread pool.
//!
//! A picture goes up as one R16_UNORM texture (a slice of a small array): the Y plane on top, then
//! U and V (side by side for 4:2:2, below one another for 4:4:4), every sample moved to the top
//! bits of its 16 so the converter scales it the way it does P010 (8 bit samples are spread over the 16 bits instead, see `next_frame`). The converter's planar path
//! (`ps_planar`) reads it; DNx is intra only, so every sample is a sync sample and seeking is
//! just moving the read position.

use std::fs::File;
use std::path::Path;

use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R16_UNORM, DXGI_SAMPLE_DESC};

use crate::d3d11::{Gpu, Planes};
use crate::decoder::{Frame, StreamInfo};
use crate::mp4::Track;
use crate::dnx::{self, DnxDecoder};
use crate::prores::Picture;
use crate::{Error, Result};

/// Slices of the upload texture; the player copies a picture out right after `next_frame`, two
/// are enough to never write one that is being read.
const SLOTS: u32 = 2;

pub struct DnxVideo {
    file: File,
    track: Track,
    gpu: Gpu,
    dec: DnxDecoder,
    pic: Picture,
    packed: Vec<u16>,
    buf: Vec<u8>,
    next_sample: usize,
    /// Size the texture was made for: picture width and height, 4:4:4.
    layout: Option<(u32, u32, bool, ID3D11Texture2D)>,
    slot: u32,
    info: StreamInfo,
}

fn gpu_err<T>(what: &str, r: windows::core::Result<T>) -> Result<T> {
    r.map_err(|e| Error::Gpu(format!("{what}: {e}")))
}

impl DnxVideo {
    /// Threads the slices of a picture are spread over.
    fn threads() -> usize {
        (std::thread::available_parallelism().map_or(4, |n| n.get()) / 2).clamp(2, 8)
    }

    pub fn open(path: &Path) -> Result<Self> {
        let mut file = File::open(path)?;
        let movie = crate::container::read(&mut file)?;
        let track = movie.video().ok_or(Error::Invalid("no video track"))?.clone();
        if !dnx::is_dnx_fourcc(&track.fourcc) {
            return Err(Error::Unsupported("not a DNxHD / DNxHR track"));
        }
        let mut buf = Vec::new();
        track.read_sample(&mut file, 0, &mut buf)?;
        let hdr = dnx::parse_header(&buf)?;
        let bits = hdr.bits;
        let gpu = Gpu::open_any()?;
        let mut vui = crate::hevc::Vui::default();
        // the header names the colour volume; these are H.273 code points
        vui.colour_primaries = hdr.colour_primaries;
        vui.transfer_characteristics = hdr.transfer;
        vui.matrix_coefficients = hdr.matrix;
        let info = StreamInfo {
            width: hdr.width,
            height: hdr.height,
            bit_depth: bits,
            frame_count: track.samples.len(),
            frame_rate: track.frame_rate(),
            timescale: track.timescale,
            rotation: track.rotation,
            vui,
            display: crate::decoder::display_size(hdr.width, hdr.height, track.par, (0, 0)),
        };
        Ok(Self { file, track, gpu, dec: DnxDecoder::new(Self::threads()), pic: Picture::default(), packed: Vec::new(), buf, next_sample: 0, layout: None, slot: 0, info })
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

    /// Continues at sample `i` (every DNx frame is a sync sample).
    pub fn seek_to_sample(&mut self, i: usize) {
        self.next_sample = i.min(self.track.samples.len());
    }

    pub fn adapter(&self) -> &str {
        &self.gpu.adapter_name
    }

    pub fn config_bitstream_raw(&self) -> Option<u32> {
        None
    }

    pub fn wait_idle(&self) -> Result<()> {
        self.gpu.wait_idle()
    }

    /// The last decoded picture as luma and interleaved chroma (for tests and thumbnails).
    pub fn read_back(&mut self, _f: &Frame) -> Result<Planes> {
        let p = &self.pic;
        let (cw, ch) = p.chroma_size();
        let (w, h) = (p.width as usize, p.height as usize);
        let mut y = Vec::with_capacity(w * h);
        for r in 0..h {
            y.extend_from_slice(&p.y[r * p.stride_y..][..w]);
        }
        let mut uv = Vec::with_capacity(cw as usize * ch as usize * 2);
        for r in 0..ch as usize {
            for x in 0..cw as usize {
                uv.push(p.u[r * p.stride_c + x]);
                uv.push(p.v[r * p.stride_c + x]);
            }
        }
        Ok(Planes { width: p.width, height: p.height, bit_depth: p.bits, y, uv })
    }

    fn texture(&mut self, w: u32, h: u32, c444: bool) -> Result<ID3D11Texture2D> {
        if let Some((lw, lh, lc, t)) = &self.layout {
            if (*lw, *lh, *lc) == (w, h, c444) {
                return Ok(t.clone());
            }
            return Err(Error::Unsupported("picture size change inside a stream"));
        }
        let d = D3D11_TEXTURE2D_DESC {
            Width: w.next_multiple_of(2),
            Height: h * if c444 { 3 } else { 2 },
            MipLevels: 1,
            ArraySize: SLOTS,
            Format: DXGI_FORMAT_R16_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut t = None;
        gpu_err("CreateTexture2D (DNx pictures)", unsafe { self.gpu.device.CreateTexture2D(&d, None, Some(&mut t)) })?;
        let t = t.ok_or(Error::Gpu("no texture".into()))?;
        self.layout = Some((w, h, c444, t.clone()));
        Ok(t)
    }

    /// The next picture; `None` at the end. A frame stays valid until the next call.
    pub fn next_frame(&mut self) -> Result<Option<Frame>> {
        if self.next_sample >= self.track.samples.len() {
            return Ok(None);
        }
        let i = self.next_sample;
        self.next_sample += 1;
        self.track.read_sample(&mut self.file, i, &mut self.buf)?;
        self.dec.decode(&self.buf, &mut self.pic)?;
        let (pw, ph, c444) = (self.pic.width, self.pic.height, self.pic.chroma_444);
        let tex = self.texture(pw, ph, c444)?;
        let p = &self.pic;
        let (w, h) = (p.width as usize, p.height as usize);
        // pack Y, U, V into the texture's layout, samples in the top bits
        let tw = w.next_multiple_of(2);
        let (cw, _) = p.chroma_size();
        let cw = cw as usize;
        let rows = if c444 { 3 * h } else { 2 * h };
        self.packed.clear();
        self.packed.resize(tw * rows, 0);
        // 10 / 12 bit samples go to the top bits (the converter scales them like P010); 8 bit ones
        // fill the 16 bits as v * 257, which is exactly v / 255 once read as normalised
        let sh = 16 - p.bits as u32;
        let up = |v: u16| if p.bits == 8 { v * 257 } else { v << sh };
        let out = &mut self.packed;
        for r in 0..h {
            let dst = &mut out[r * tw..][..w];
            for (d, s) in dst.iter_mut().zip(&p.y[r * p.stride_y..][..w]) {
                *d = up(*s);
            }
        }
        for r in 0..h {
            if c444 {
                let u = &mut out[(h + r) * tw..][..w];
                for (d, s) in u.iter_mut().zip(&p.u[r * p.stride_c..][..w]) {
                    *d = up(*s);
                }
                let v = &mut out[(2 * h + r) * tw..][..w];
                for (d, s) in v.iter_mut().zip(&p.v[r * p.stride_c..][..w]) {
                    *d = up(*s);
                }
            } else {
                let row = &mut out[(h + r) * tw..][..tw];
                for x in 0..cw {
                    row[x] = up(p.u[r * p.stride_c + x]);
                    row[cw + x] = up(p.v[r * p.stride_c + x]);
                }
            }
        }
        let slot = self.slot;
        self.slot = (self.slot + 1) % SLOTS;
        unsafe {
            self.gpu.context.UpdateSubresource(&tex, slot, None, self.packed.as_ptr() as *const _, (tw * 2) as u32, 0);
        }
        let s = &self.track.samples[i];
        Ok(Some(Frame { index: slot, texture: tex, poc: i as i32, pts: s.pts, width: p.width, height: p.height, crop: (0, 0, 0, 0), bit_depth: p.bits }))
    }
}
