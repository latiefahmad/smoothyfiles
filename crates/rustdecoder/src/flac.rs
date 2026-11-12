//! FLAC decoder, written from the format description (xiph.org/flac/format.html).
//!
//! Windows' own FLAC transform was tried first and does not work for this use: after two
//! frames `ProcessOutput` blocks for good (checked on this machine with several clips and
//! every input type variant), so FLAC is decoded here. It is small, and bit exact by
//! construction: samples are integers all the way, then divided by 2^(bits-1) like ffmpeg does.
//!
//! One call decodes one frame (what a Matroska block / MP4 sample holds). CRCs are not
//! checked.

use crate::{Error, Result};

/// What `STREAMINFO` says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits: u16,
}

/// Reads `STREAMINFO` from `fLaC` + metadata blocks (a Matroska `CodecPrivate`).
pub fn stream_info(config: &[u8]) -> Option<StreamInfo> {
    let mut b = config.strip_prefix(b"fLaC").unwrap_or(config);
    // metadata blocks: 1 bit last, 7 bit type, 24 bit length
    while b.len() >= 4 {
        let (ty, len) = (b[0] & 0x7f, u32::from_be_bytes([0, b[1], b[2], b[3]]) as usize);
        let body = b.get(4..4 + len)?;
        if ty == 0 {
            let d = body.get(..18)?;
            let rate = (d[10] as u32) << 12 | (d[11] as u32) << 4 | (d[12] as u32) >> 4;
            let channels = ((d[12] >> 1) & 7) as u16 + 1;
            let bits = (((d[12] & 1) as u16) << 4 | (d[13] >> 4) as u16) + 1;
            return Some(StreamInfo { sample_rate: rate, channels, bits });
        }
        b = &b[4 + len..];
    }
    None
}

struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn left(&self) -> usize {
        (self.d.len() * 8).saturating_sub(self.pos)
    }

    fn read(&mut self, n: u32) -> Result<u32> {
        debug_assert!(n <= 32);
        if self.left() < n as usize {
            return Err(Error::Invalid("FLAC frame ends early"));
        }
        let mut v = 0u64;
        let mut need = n;
        while need > 0 {
            let byte = self.d[self.pos >> 3] as u64;
            let avail = 8 - (self.pos & 7) as u32;
            let take = need.min(avail);
            v = (v << take) | ((byte >> (avail - take)) & ((1 << take) - 1));
            self.pos += take as usize;
            need -= take;
        }
        Ok(v as u32)
    }

    fn signed(&mut self, n: u32) -> Result<i64> {
        if n == 0 {
            return Ok(0);
        }
        let v = self.read(n)? as i64;
        Ok((v << (64 - n)) >> (64 - n))
    }

    fn unary(&mut self) -> Result<u32> {
        let mut n = 0;
        while self.read(1)? == 0 {
            n += 1;
            if n > (1 << 24) {
                return Err(Error::Invalid("FLAC unary code"));
            }
        }
        Ok(n)
    }

    fn align(&mut self) {
        self.pos = (self.pos + 7) & !7;
    }
}

pub struct FlacDecoder {
    pub info: StreamInfo,
    chans: Vec<Vec<i32>>,
}

impl FlacDecoder {
    pub fn new(info: StreamInfo) -> Result<Self> {
        if info.channels == 0 || info.channels > 8 || !(4..=32).contains(&info.bits) {
            return Err(Error::Unsupported("FLAC stream parameters"));
        }
        Ok(FlacDecoder { info, chans: Vec::new() })
    }

    /// Decodes the frame in `data`, appending interleaved `f32` to `out`. Returns the frames
    /// (samples per channel) added.
    pub fn decode(&mut self, data: &[u8], out: &mut Vec<f32>) -> Result<usize> {
        let mut r = Bits { d: data, pos: 0 };
        if r.read(14)? != 0x3FFE {
            return Err(Error::Invalid("FLAC frame sync"));
        }
        r.read(2)?; // reserved, blocking strategy
        let bs_code = r.read(4)?;
        let sr_code = r.read(4)?;
        let ch_code = r.read(4)?;
        let bps_code = r.read(3)?;
        r.read(1)?;
        // frame / sample number, UTF-8 style
        let first = r.read(8)?;
        let extra = if first < 0x80 { 0 } else { (first as u8).leading_ones().saturating_sub(1) };
        if first >= 0x80 && !(1..=6).contains(&extra) {
            return Err(Error::Invalid("FLAC frame number"));
        }
        for _ in 0..extra {
            r.read(8)?;
        }
        let bs = match bs_code {
            0 => return Err(Error::Invalid("FLAC block size")),
            1 => 192,
            2..=5 => 576usize << (bs_code - 2),
            6 => r.read(8)? as usize + 1,
            7 => r.read(16)? as usize + 1,
            _ => 256usize << (bs_code - 8),
        };
        match sr_code {
            12 => {
                r.read(8)?;
            }
            13 | 14 => {
                r.read(16)?;
            }
            15 => return Err(Error::Invalid("FLAC sample rate code")),
            _ => {}
        }
        let bps = match bps_code {
            0 => self.info.bits as u32,
            1 => 8,
            2 => 12,
            4 => 16,
            5 => 20,
            6 => 24,
            7 => 32,
            _ => return Err(Error::Invalid("FLAC sample size")),
        };
        r.read(8)?; // header CRC-8
        let n_ch = match ch_code {
            0..=7 => ch_code as usize + 1,
            8..=10 => 2,
            _ => return Err(Error::Invalid("FLAC channel assignment")),
        };
        if n_ch != self.info.channels as usize {
            return Err(Error::Unsupported("FLAC channel count changes"));
        }
        self.chans.resize_with(n_ch, Vec::new);
        for c in 0..n_ch {
            // the side channel has one more bit
            let extra_bit = matches!((ch_code, c), (8, 1) | (9, 0) | (10, 1));
            let buf = &mut self.chans[c];
            buf.clear();
            buf.resize(bs, 0);
            subframe(&mut r, buf, bps + extra_bit as u32)?;
        }
        r.align();
        match ch_code {
            8 => {
                let (l, rest) = self.chans.split_at_mut(1);
                for (l, s) in l[0].iter().zip(rest[0].iter_mut()) {
                    *s = l.wrapping_sub(*s);
                }
            }
            9 => {
                let (s, rest) = self.chans.split_at_mut(1);
                for (s, r) in s[0].iter_mut().zip(rest[0].iter()) {
                    *s = s.wrapping_add(*r);
                }
            }
            10 => {
                let (m, s) = self.chans.split_at_mut(1);
                for (m, s) in m[0].iter_mut().zip(s[0].iter_mut()) {
                    let mid = ((*m as i64) << 1) | (*s as i64 & 1);
                    let side = *s as i64;
                    *m = ((mid + side) >> 1) as i32;
                    *s = ((mid - side) >> 1) as i32;
                }
            }
            _ => {}
        }
        let scale = 1.0 / (1u64 << (bps - 1)) as f64;
        out.reserve(bs * n_ch);
        for i in 0..bs {
            for c in &self.chans {
                out.push((c[i] as f64 * scale) as f32);
            }
        }
        Ok(bs)
    }
}

const FIXED: [&[i64]; 5] = [&[], &[1], &[2, -1], &[3, -3, 1], &[4, -6, 4, -1]];

fn subframe(r: &mut Bits, out: &mut [i32], bps: u32) -> Result<()> {
    if r.read(1)? != 0 {
        return Err(Error::Invalid("FLAC subframe padding"));
    }
    let ty = r.read(6)?;
    let wasted = if r.read(1)? == 1 { r.unary()? + 1 } else { 0 };
    if wasted >= bps {
        return Err(Error::Invalid("FLAC wasted bits"));
    }
    let bps = bps - wasted;
    let n = out.len();
    let mut x = vec![0i64; n];
    match ty {
        0 => {
            let v = r.signed(bps)?;
            x.fill(v);
        }
        1 => {
            for s in x.iter_mut() {
                *s = r.signed(bps)?;
            }
        }
        8..=12 => {
            let order = (ty - 8) as usize;
            if order > n {
                return Err(Error::Invalid("FLAC predictor order"));
            }
            for s in x.iter_mut().take(order) {
                *s = r.signed(bps)?;
            }
            residual(r, &mut x, order)?;
            let c = FIXED[order];
            for i in order..n {
                let mut p = 0i64;
                for (j, k) in c.iter().enumerate() {
                    p += k * x[i - 1 - j];
                }
                x[i] += p;
            }
        }
        32..=63 => {
            let order = (ty - 31) as usize;
            if order > n {
                return Err(Error::Invalid("FLAC predictor order"));
            }
            for s in x.iter_mut().take(order) {
                *s = r.signed(bps)?;
            }
            let prec = r.read(4)? + 1;
            if prec == 16 {
                return Err(Error::Invalid("FLAC coefficient precision"));
            }
            let shift = r.signed(5)?;
            if shift < 0 {
                return Err(Error::Invalid("FLAC negative shift"));
            }
            let mut coef = Vec::with_capacity(order);
            for _ in 0..order {
                coef.push(r.signed(prec)?);
            }
            residual(r, &mut x, order)?;
            for i in order..n {
                let mut p = 0i64;
                for (j, k) in coef.iter().enumerate() {
                    p += k * x[i - 1 - j];
                }
                x[i] += p >> shift;
            }
        }
        _ => return Err(Error::Invalid("FLAC subframe type")),
    }
    for (o, v) in out.iter_mut().zip(x) {
        *o = (v << wasted) as i32;
    }
    Ok(())
}

/// Rice coded residual for `x[order..]` (stored in place).
fn residual(r: &mut Bits, x: &mut [i64], order: usize) -> Result<()> {
    let method = r.read(2)?;
    if method > 1 {
        return Err(Error::Invalid("FLAC residual method"));
    }
    let (pbits, escape) = if method == 0 { (4, 15) } else { (5, 31) };
    let porder = r.read(4)?;
    let n = x.len();
    let parts = 1usize << porder;
    if n >> porder < order || n % parts != 0 {
        return Err(Error::Invalid("FLAC partition order"));
    }
    let mut i = order;
    for p in 0..parts {
        let count = (n >> porder) - if p == 0 { order } else { 0 };
        let param = r.read(pbits)?;
        if i + count > n {
            return Err(Error::Invalid("FLAC partition size"));
        }
        if param == escape {
            let bits = r.read(5)?;
            for _ in 0..count {
                x[i] = r.signed(bits)?;
                i += 1;
            }
        } else {
            for _ in 0..count {
                let q = r.unary()? as u64;
                let low = if param > 0 { r.read(param)? as u64 } else { 0 };
                let v = (q << param) | low;
                x[i] = ((v >> 1) as i64) ^ -((v & 1) as i64);
                i += 1;
            }
        }
    }
    Ok(())
}
