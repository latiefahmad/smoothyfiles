//! Apple ProRes on the CPU, written from the public description of the format (SMPTE RDD 36).
//!
//! ProRes is intra only: every frame is a list of independent slices (a row of 1, 2, 4 or 8
//! macroblocks each), so the slices of a frame decode in parallel. A slice holds three (four
//! with alpha) entropy coded planes of 8x8 DCT blocks: the DC coefficients first (delta coded
//! from block to block), then the AC coefficients as (run, level) pairs walked through the
//! blocks of the slice one scan position at a time. Both use adaptive Rice / Exp-Golomb hybrid
//! codes whose codebook follows the previous values. After the slice's quantiser scale and the
//! frame's two quantisation matrices are applied, an 8x8 inverse DCT gives the samples.
//!
//! Output is planar 4:2:2 or 4:4:4, 10 bit for the 422 profiles (Proxy, LT, Standard, HQ) and 12 bit for 4444
//! (the sample depth the profile is made for), one `u16` per sample, least significant bits.
//! Interlaced frames carry two pictures (fields) that are woven into the frame. The alpha plane
//! of 4444 is skipped (its bytes are stepped over).

use crate::{Error, Result};

const FIRST_DC_CB: u8 = 0xB8;
const DC_CB: [u8; 7] = [0x04, 0x28, 0x28, 0x4D, 0x4D, 0x70, 0x70];
const RUN_CB: [u8; 16] = [0x06, 0x06, 0x05, 0x05, 0x04, 0x29, 0x29, 0x29, 0x29, 0x28, 0x28, 0x28, 0x28, 0x28, 0x28, 0x4C];
const LEVEL_CB: [u8; 10] = [0x04, 0x0A, 0x05, 0x06, 0x04, 0x28, 0x28, 0x28, 0x28, 0x4C];

/// Coefficient positions (raster index inside the block) in the order they are coded.
const SCAN_PROGRESSIVE: [u8; 64] = [
    0, 1, 8, 9, 2, 3, 10, 11, 16, 17, 24, 25, 18, 19, 26, 27, 4, 5, 12, 20, 13, 6, 7, 14, 21, 28, 29, 22, 15, 23, 30, 31, 32, 33, 40, 48, 41, 34, 35, 42, 49, 56, 57, 50, 43, 36, 37, 44, 51, 58, 59, 52,
    45, 38, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];
const SCAN_INTERLACED: [u8; 64] = [
    0, 8, 1, 9, 16, 24, 17, 25, 2, 10, 3, 11, 18, 26, 19, 27, 32, 40, 33, 34, 41, 48, 56, 49, 42, 35, 43, 50, 57, 58, 51, 59, 4, 12, 5, 6, 13, 20, 28, 21, 14, 7, 15, 22, 29, 36, 44, 37, 30, 23, 31, 38,
    45, 52, 60, 53, 46, 39, 47, 54, 61, 62, 55, 63,
];

/// What the frame header says.
#[derive(Debug, Clone)]
pub struct FrameHeader {
    pub width: u32,
    pub height: u32,
    /// 4:4:4 (true) or 4:2:2 (false).
    pub chroma_444: bool,
    /// 0 progressive, 1 interlaced with the top field first, 2 bottom field first.
    pub interlace: u8,
    /// Alpha channel: 0 none, 1 8 bit, 2 16 bit.
    pub alpha: u8,
    pub qmat_luma: [u8; 64],
    pub qmat_chroma: [u8; 64],
    pub colour_primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
    /// Bytes from the start of the frame to the first picture header.
    pub header_end: usize,
}

fn be16(b: &[u8], i: usize) -> Result<usize> {
    b.get(i..i + 2).map(|x| u16::from_be_bytes([x[0], x[1]]) as usize).ok_or(Error::Invalid("ProRes header cut short"))
}
fn be32(b: &[u8], i: usize) -> Result<usize> {
    b.get(i..i + 4).map(|x| u32::from_be_bytes([x[0], x[1], x[2], x[3]]) as usize).ok_or(Error::Invalid("ProRes header cut short"))
}

/// Reads the frame header (`frame` starts at the frame size field).
pub fn parse_header(frame: &[u8]) -> Result<FrameHeader> {
    if frame.len() < 28 || &frame[4..8] != b"icpf" {
        return Err(Error::Invalid("not a ProRes frame"));
    }
    let h = &frame[8..];
    let hdr_size = be16(h, 0)?;
    if hdr_size < 20 || hdr_size > h.len() {
        return Err(Error::Invalid("ProRes frame header size"));
    }
    let width = be16(h, 8)? as u32;
    let height = be16(h, 10)? as u32;
    if width > 8192 || height > 8192 {
        return Err(Error::Unsupported("ProRes frame larger than 8192 pixels"));
    }
    if width == 0 || height == 0 {
        return Err(Error::Invalid("ProRes frame size is zero"));
    }
    let flags = h[12];
    let chroma = flags >> 6;
    let chroma_444 = match chroma {
        2 => false,
        3 => true,
        _ => return Err(Error::Invalid("ProRes chroma format")),
    };
    let interlace = (flags >> 2) & 3;
    if interlace > 2 {
        return Err(Error::Invalid("ProRes interlace mode"));
    }
    let alpha = h[17] & 0xF;
    let load = h[19];
    let mut at = 20;
    let mut qmat_luma = [4u8; 64];
    let mut qmat_chroma = [4u8; 64];
    if load & 2 != 0 {
        qmat_luma.copy_from_slice(h.get(at..at + 64).ok_or(Error::Invalid("ProRes quantisation matrix cut short"))?);
        at += 64;
        qmat_chroma = qmat_luma;
    }
    if load & 1 != 0 {
        qmat_chroma.copy_from_slice(h.get(at..at + 64).ok_or(Error::Invalid("ProRes quantisation matrix cut short"))?);
    }
    Ok(FrameHeader { width, height, chroma_444, interlace, alpha, qmat_luma, qmat_chroma, colour_primaries: h[14], transfer: h[15], matrix: h[16], header_end: 8 + hdr_size })
}

/// A decoded frame: planar, `bits` per sample in the low bits of each `u16`. The planes have
/// the coded size (whole macroblocks); the picture is the top left `width` x `height`.
#[derive(Debug, Clone, Default)]
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub bits: u8,
    pub chroma_444: bool,
    pub stride_y: usize,
    pub stride_c: usize,
    /// Rows of the coded planes.
    pub rows: usize,
    pub y: Vec<u16>,
    pub u: Vec<u16>,
    pub v: Vec<u16>,
}

impl Picture {
    /// Chroma plane width and height of the visible picture.
    pub fn chroma_size(&self) -> (u32, u32) {
        if self.chroma_444 { (self.width, self.height) } else { (self.width.div_ceil(2), self.height) }
    }

    fn prepare(&mut self, h: &FrameHeader, bits: u8) {
        let mb_w = (h.width as usize).div_ceil(16);
        let mb_h = if h.interlace == 0 { (h.height as usize).div_ceil(16) } else { (h.height as usize).div_ceil(32) * 2 };
        self.width = h.width;
        self.height = h.height;
        self.bits = bits;
        self.chroma_444 = h.chroma_444;
        self.rows = mb_h * 16;
        self.stride_y = mb_w * 16;
        self.stride_c = if h.chroma_444 { mb_w * 16 } else { mb_w * 8 };
        self.y.resize(self.stride_y * self.rows, 0);
        self.u.resize(self.stride_c * self.rows, 0);
        self.v.resize(self.stride_c * self.rows, 0);
    }
}

// ---------------------------------------------------------------------------------------------
// entropy decoding

struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
    end: usize,
}

impl<'a> Bits<'a> {
    fn new(d: &'a [u8]) -> Self {
        Bits { d, pos: 0, end: d.len() * 8 }
    }

    /// The next 57 bits or more, left aligned in a `u64` (zeros past the end).
    #[inline(always)]
    fn window(&self) -> u64 {
        let byte = self.pos >> 3;
        let v = if byte + 8 <= self.d.len() {
            u64::from_be_bytes(self.d[byte..byte + 8].try_into().unwrap())
        } else {
            let mut t = [0u8; 8];
            if byte < self.d.len() {
                let n = self.d.len() - byte;
                t[..n].copy_from_slice(&self.d[byte..]);
            }
            u64::from_be_bytes(t)
        };
        v << (self.pos & 7)
    }

    #[inline(always)]
    fn left(&self) -> usize {
        self.end.saturating_sub(self.pos)
    }

    /// One codeword of the Rice / Exp-Golomb hybrid with codebook `cb`:
    /// bits 5-7 the Rice order, bits 2-4 the Exp-Golomb order, bits 0-1 how many prefix
    /// values stay Rice coded.
    #[inline(always)]
    fn codeword(&mut self, cb: u8) -> Result<u32> {
        let rice = (cb >> 5) as u32;
        let exp = ((cb >> 2) & 7) as u32;
        let t = (cb & 3) as u32 + SWITCH_BIAS;
        let w = self.window();
        let q = w.leading_zeros();
        if q <= t {
            let v = if rice == 0 { 0 } else { ((w << (q + 1)) >> (64 - rice)) as u32 };
            self.pos += (q + 1 + rice) as usize;
            Ok((q << rice) + v)
        } else {
            let k = q - t - 1;
            let n = k + exp;
            if q + 1 + n > 56 || n > 32 {
                return Err(Error::Invalid("ProRes code too long"));
            }
            let payload = if n == 0 { 0 } else { ((w << (q + 1)) >> (64 - n)) as u32 };
            self.pos += (q + 1 + n) as usize;
            Ok(((t + 1) << rice).wrapping_add((((1u64 << k) - 1) << exp) as u32).wrapping_add(payload))
        }
    }

    #[inline(always)]
    fn bit(&mut self) -> u32 {
        let b = (self.window() >> 63) as u32;
        self.pos += 1;
        b
    }

    /// Nothing but zero bits left (the padding of a slice).
    fn rest_is_zero(&self) -> bool {
        let n = self.left();
        n == 0 || (n < 64 && (self.window() >> (64 - n)) == 0)
    }
}

const SWITCH_BIAS: u32 = 0;

/// Decodes one plane of a slice into `coef` (`blocks` x 64, zeroed by the caller).
fn read_coefficients(data: &[u8], blocks: usize, scan: &[u8; 64], coef: &mut [i32]) -> Result<()> {
    let mut br = Bits::new(data);
    // DC: the first block directly, the rest as differences
    let c = br.codeword(FIRST_DC_CB)?;
    let mut prev = ((c >> 1) as i32) ^ -((c & 1) as i32);
    coef[0] = prev;
    let mut code = 5u32;
    let mut sign = 0i32;
    for b in 1..blocks {
        code = br.codeword(DC_CB[code.min(6) as usize])?;
        if code != 0 {
            sign ^= (code & 1) as i32;
        } else {
            sign = 0;
        }
        prev = prev.wrapping_add(((((code.wrapping_add(1)) >> 1) as i32) ^ -sign).wrapping_add(sign));
        coef[b * 64] = prev;
    }
    // AC
    let log2 = blocks.trailing_zeros();
    let mask = blocks - 1;
    let total = blocks * 64;
    let mut pos = mask;
    let mut run = 4u32;
    let mut level = 2u32;
    while !br.rest_is_zero() {
        run = br.codeword(RUN_CB[run.min(15) as usize])?;
        level = br.codeword(LEVEL_CB[level.min(9) as usize])?.wrapping_add(1);
        pos += run as usize + 1;
        if pos >= total {
            return Err(Error::Invalid("ProRes coefficient position past the block"));
        }
        let s = br.bit();
        let v = level as i32;
        coef[((pos & mask) << 6) + scan[pos >> log2] as usize] = if s != 0 { -v } else { v };
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// inverse DCT

// cos(k pi / 16) * sqrt(2) * 2^16
const W1: i64 = 90901;
const W2: i64 = 85627;
const W3: i64 = 77062;
const W4: i64 = 65536;
const W5: i64 = 51491;
const W6: i64 = 35468;
const W7: i64 = 18081;
const ROW_SHIFT: i32 = 17;

/// One 8-point inverse transform (the even part from inputs 0, 2, 4, 6 and the odd part from
/// 1, 3, 5, 7), results shifted right by `shift` after adding `round`.
#[inline(always)]
fn idct8(x: [i64; 8], shift: i32, round: i64) -> [i64; 8] {
    let a = round + W4 * x[0];
    let (mut a0, mut a1, mut a2, mut a3) = (a, a, a, a);
    a0 += W2 * x[2] + W4 * x[4] + W6 * x[6];
    a1 += W6 * x[2] - W4 * x[4] - W2 * x[6];
    a2 += -W6 * x[2] - W4 * x[4] + W2 * x[6];
    a3 += -W2 * x[2] + W4 * x[4] - W6 * x[6];
    let b0 = W1 * x[1] + W3 * x[3] + W5 * x[5] + W7 * x[7];
    let b1 = W3 * x[1] - W7 * x[3] - W1 * x[5] - W5 * x[7];
    let b2 = W5 * x[1] - W1 * x[3] + W7 * x[5] + W3 * x[7];
    let b3 = W7 * x[1] - W5 * x[3] + W3 * x[5] - W1 * x[7];
    [(a0 + b0) >> shift, (a1 + b1) >> shift, (a2 + b2) >> shift, (a3 + b3) >> shift, (a3 - b3) >> shift, (a2 - b2) >> shift, (a1 - b1) >> shift, (a0 - b0) >> shift]
}

/// 8x8 inverse DCT of dequantised coefficients, in place (rows, then columns). The coefficients
/// are scaled for samples of `bits` bits (the result is 1/32 of the DCT sum at 10 bit, 1/8 at 12
/// bit); the result is the sample offset from the middle of the range, not yet clipped.
pub(crate) fn idct(b: &mut [i32; 64], bits: u8) {
    let col_shift = 30 - bits as i32;
    for r in 0..8 {
        let o = r * 8;
        let x: [i64; 8] = std::array::from_fn(|i| b[o + i] as i64);
        let y = idct8(x, ROW_SHIFT, 1 << (ROW_SHIFT - 1));
        for i in 0..8 {
            b[o + i] = y[i] as i32;
        }
    }
    for c in 0..8 {
        let x: [i64; 8] = std::array::from_fn(|i| b[i * 8 + c] as i64);
        let y = idct8(x, col_shift, 1 << (col_shift - 1));
        for i in 0..8 {
            b[i * 8 + c] = y[i] as i32;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// slices

#[derive(Clone, Copy)]
struct Job {
    /// First macroblock column, row (rows of the picture, i.e. of the field), count.
    mb_x: usize,
    mb_y: usize,
    mbs: usize,
    /// Offset and length of the slice in the frame.
    at: usize,
    len: usize,
    /// 0 for progressive frames and the first field, 1 for the second field.
    parity: usize,
}

#[derive(Clone, Copy)]
struct PlanePtr {
    p: *mut u16,
    stride: usize,
}
// SAFETY: slices write disjoint macroblocks.
unsafe impl Send for PlanePtr {}
unsafe impl Sync for PlanePtr {}

struct Ctx<'a> {
    frame: &'a [u8],
    hdr: &'a FrameHeader,
    scan: &'a [u8; 64],
    bits: u8,
    planes: [PlanePtr; 3],
    interlaced: bool,
    /// Dequantisation: quantisation matrices before the slice's scale.
    qmat: [&'a [u8; 64]; 3],
}

fn decode_slice(c: &Ctx, j: &Job) -> Result<()> {
    let s = c.frame.get(j.at..j.at + j.len).ok_or(Error::Invalid("ProRes slice outside the frame"))?;
    if s.len() < 6 {
        return Err(Error::Invalid("ProRes slice too short"));
    }
    let hs = (s[0] >> 3) as usize;
    if hs < 6 || hs > s.len() {
        return Err(Error::Invalid("ProRes slice header size"));
    }
    let qi = s[1] as i32;
    let qscale = if qi > 128 { (qi - 96) << 2 } else { qi };
    let ysz = be16(s, 2)?;
    let usz = be16(s, 4)?;
    let vsz = if hs > 7 { be16(s, 6)? } else { s.len().checked_sub(hs + ysz + usz).ok_or(Error::Invalid("ProRes slice sizes"))? };
    if hs + ysz + usz + vsz > s.len() {
        return Err(Error::Invalid("ProRes slice sizes"));
    }
    let parts = [&s[hs..hs + ysz], &s[hs + ysz..hs + ysz + usz], &s[hs + ysz + usz..hs + ysz + usz + vsz]];
    let (bpm_luma, bpm_chroma) = if c.hdr.chroma_444 { (4, 4) } else { (4, 2) };
    let mut coef = vec![0i32; j.mbs * 4 * 64];
    for (plane, data) in parts.iter().enumerate() {
        let bpm = if plane == 0 { bpm_luma } else { bpm_chroma };
        let blocks = j.mbs * bpm;
        let coef = &mut coef[..blocks * 64];
        coef.fill(0);
        read_coefficients(data, blocks, c.scan, coef)?;
        let qm = c.qmat[plane];
        let pp = c.planes[plane];
        let (mb_px_w, mb_px_h) = if plane == 0 || c.hdr.chroma_444 { (16, 16) } else { (8, 16) };
        let mid = 1i32 << (c.bits - 1);
        let lo = 4i32;
        let hi = (1i32 << c.bits) - 5;
        for mb in 0..j.mbs {
            let x0 = (j.mb_x + mb) * mb_px_w;
            let y0 = j.mb_y * mb_px_h;
            for k in 0..bpm {
                let (dx, dy) = if bpm == 4 && plane == 0 { [(0, 0), (8, 0), (0, 8), (8, 8)][k] } else if bpm == 4 { [(0, 0), (0, 8), (8, 0), (8, 8)][k] } else { [(0, 0), (0, 8)][k] };
                let mut blk = [0i32; 64];
                let src = &coef[(mb * bpm + k) * 64..][..64];
                for i in 0..64 {
                    blk[i] = src[i].wrapping_mul(qm[i] as i32).wrapping_mul(qscale);
                }
                idct(&mut blk, c.bits);
                for r in 0..8 {
                    let y = y0 + dy + r;
                    let row = if c.interlaced { 2 * y + j.parity } else { y };
                    // SAFETY: this macroblock belongs to this slice alone and lies inside the plane
                    unsafe {
                        let dst = pp.p.add(row * pp.stride + x0 + dx);
                        for x in 0..8 {
                            *dst.add(x) = blk[r * 8 + x].wrapping_add(mid).clamp(lo, hi) as u16;
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Decodes ProRes frames. Slices run on a thread pool of the size given at creation.
pub struct ProResDecoder {
    pool: Option<rayon::ThreadPool>,
    threads: usize,
}

impl ProResDecoder {
    /// `threads` of 1 decodes on the calling thread; 0 uses every logical core.
    pub fn new(threads: usize) -> Self {
        let threads = if threads == 0 { std::thread::available_parallelism().map_or(4, |n| n.get()) } else { threads };
        let pool = if threads > 1 { rayon::ThreadPoolBuilder::new().num_threads(threads).thread_name(|i| format!("prores-{i}")).build().ok() } else { None };
        ProResDecoder { pool, threads }
    }

    pub fn threads(&self) -> usize {
        if self.pool.is_some() { self.threads } else { 1 }
    }

    /// Decodes `frame` (one sample of the MOV) into `out`. `bits` is the sample depth of the
    /// profile: 10 for 422 profiles, 12 for 4444.
    pub fn decode(&self, frame: &[u8], bits: u8, out: &mut Picture) -> Result<FrameHeader> {
        let hdr = parse_header(frame)?;
        if !(8..=12).contains(&bits) {
            return Err(Error::Unsupported("ProRes sample depth"));
        }
        out.prepare(&hdr, bits);
        let mb_w = (hdr.width as usize).div_ceil(16);
        let mb_h = if hdr.interlace == 0 { (hdr.height as usize).div_ceil(16) } else { (hdr.height as usize).div_ceil(32) };
        let pictures = if hdr.interlace == 0 { 1 } else { 2 };
        let mut jobs: Vec<Job> = Vec::new();
        let mut at = hdr.header_end;
        for p in 0..pictures {
            let ph = frame.get(at..).ok_or(Error::Invalid("ProRes picture missing"))?;
            let ph_size = (*ph.first().ok_or(Error::Invalid("ProRes picture header cut short"))? >> 3) as usize;
            let pic_size = be32(ph, 1)?;
            let n = be16(ph, 5)?;
            let log2 = ph.get(7).ok_or(Error::Invalid("ProRes picture header cut short"))? >> 4;
            if ph_size < 8 || pic_size < ph_size + 2 * n || at + pic_size > frame.len() {
                return Err(Error::Invalid("ProRes picture header"));
            }
            let mut sizes = Vec::with_capacity(n);
            for i in 0..n {
                sizes.push(be16(ph, ph_size + 2 * i)?);
            }
            let mut data = at + ph_size + 2 * n;
            let mut idx = 0;
            let parity = if p == 0 { (hdr.interlace == 2) as usize } else { (hdr.interlace == 1) as usize };
            for mb_y in 0..mb_h {
                let mut mb_x = 0;
                let mut w = 1usize << log2.min(3);
                while mb_x < mb_w {
                    while w > mb_w - mb_x {
                        w >>= 1;
                    }
                    let len = *sizes.get(idx).ok_or(Error::Invalid("ProRes slice count"))?;
                    jobs.push(Job { mb_x, mb_y, mbs: w, at: data, len, parity });
                    data += len;
                    idx += 1;
                    mb_x += w;
                }
            }
            if idx != n || data > at + pic_size {
                return Err(Error::Invalid("ProRes slice table"));
            }
            at += pic_size;
        }
        let ctx = Ctx {
            frame,
            hdr: &hdr,
            scan: if hdr.interlace == 0 { &SCAN_PROGRESSIVE } else { &SCAN_INTERLACED },
            bits,
            planes: [
                PlanePtr { p: out.y.as_mut_ptr(), stride: out.stride_y },
                PlanePtr { p: out.u.as_mut_ptr(), stride: out.stride_c },
                PlanePtr { p: out.v.as_mut_ptr(), stride: out.stride_c },
            ],
            interlaced: hdr.interlace != 0,
            qmat: [&hdr.qmat_luma, &hdr.qmat_chroma, &hdr.qmat_chroma],
        };
        let result: Result<()> = match &self.pool {
            Some(pool) => {
                use rayon::prelude::*;
                pool.install(|| jobs.par_iter().try_for_each(|j| decode_slice(&ctx, j)))
            }
            None => jobs.iter().try_for_each(|j| decode_slice(&ctx, j)),
        };
        result?;
        Ok(hdr)
    }
}

/// Whether a sample entry's four letters name a ProRes variant.
pub fn is_prores_fourcc(f: &[u8; 4]) -> bool {
    matches!(f, b"apch" | b"apcn" | b"apcs" | b"apco" | b"ap4h" | b"ap4x")
}

/// The sample depth the variant is decoded at: 12 for 4444, 10 otherwise.
pub fn bits_for_fourcc(f: &[u8; 4]) -> u8 {
    if matches!(f, b"ap4h" | b"ap4x") { 12 } else { 10 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_are_permutations() {
        for s in [&SCAN_PROGRESSIVE, &SCAN_INTERLACED] {
            let mut seen = [false; 64];
            for &p in s.iter() {
                assert!(!seen[p as usize]);
                seen[p as usize] = true;
            }
        }
    }
}
