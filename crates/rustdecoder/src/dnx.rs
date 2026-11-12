//! Avid DNxHD and DNxHR (VC-3) on the CPU, written from the published standard (SMPTE ST 2019-1).
//!
//! VC-3 is intra only. A frame is one coding unit (progressive) or two (interlaced, one per
//! field). A coding unit is a 640 byte (or longer, for tall pictures) header, an index of where
//! every macroblock row starts, and the rows themselves. Rows are independent (the DC
//! prediction restarts on each one), so they decode in parallel. A macroblock is 16x16 samples
//! cut into 8x8 blocks: 8 for 4:2:2, 12 for 4:4:4. Each block is a DC code (a bit count, then
//! that many bits as a difference from the previous block of the same component) and AC codes
//! (an amplitude codeword with a sign, an optional amplitude index and an optional zero run),
//! ended by an end-of-block code. The codeword tables ([`crate::dnxtables`]) depend on the
//! compression ID (CID). Coefficients are scaled by the macroblock's quantiser and the CID's
//! weights, then an 8x8 inverse DCT gives the samples.
//!
//! Where the standard's text and real streams part ways, this follows the streams, measured
//! against ffmpeg's decoder with hand made frames (a single coefficient of known amplitude per
//! block): (1) the reconstruction point of a quantised level `a` is `(a + 3/4)` steps, not the
//! `(a + 1/2)` of eq. 8.1; (2) CID 1271 (DNxHR HQX) uses the weights of Table D.4, not D.1.
//! 12 bit samples come out of the same transform four times larger (Table 14: the 12 bit DC range
//! is a quarter of what the ortho-normal 8x8 DCT of 12 bit samples would give), checked the same way.
//!
//! Output is planar `u16` in the shape [`crate::prores::Picture`] has, 4:2:2 or 4:4:4, 8, 10 or
//! 12 bit. Not covered: the RGB colour format (CLF = 1), 4:2:0, alpha, and CID 1260 (macroblock
//! adaptive field / frame coding). MXF, DNxHD's usual wrapper, is not read either: only MOV.

use crate::dnxtables::*;
use crate::prores::{idct, Picture};
use crate::{Error, Result};

/// Fig. 48: position in the bitstream order of the coefficient at row `v`, column `u`.
const ZIGZAG_AT: [[u8; 8]; 8] = [
    [0, 1, 5, 6, 14, 15, 27, 28],
    [2, 4, 7, 13, 16, 26, 29, 42],
    [3, 8, 12, 17, 25, 30, 41, 43],
    [9, 11, 18, 24, 31, 40, 44, 53],
    [10, 19, 23, 32, 39, 45, 52, 54],
    [20, 22, 33, 38, 46, 51, 55, 60],
    [21, 34, 37, 47, 50, 56, 59, 61],
    [35, 36, 48, 49, 57, 58, 62, 63],
];

/// Raster index (row * 8 + column) of the coefficient with bitstream index `r`.
const SCAN: [u8; 64] = {
    let mut s = [0u8; 64];
    let mut v = 0;
    while v < 8 {
        let mut u = 0;
        while u < 8 {
            s[ZIGZAG_AT[v][u] as usize] = (v * 8 + u) as u8;
            u += 1;
        }
        v += 1;
    }
    s
};

/// Whether a sample entry's four letters name DNxHD / DNxHR in a MOV.
pub fn is_dnx_fourcc(f: &[u8; 4]) -> bool {
    matches!(f, b"AVdn" | b"AVdh")
}

/// What one compression ID needs for decoding.
struct Cid {
    set: usize,
    weights_luma: &'static [u8; 64],
    weights_chroma: &'static [u8; 64],
    /// The inverse quantisation parameter p of eq. 8.1.
    p: i64,
}

fn cid_params(cid: u32) -> Option<Cid> {
    let (set, wl, wc) = match cid {
        1235 => (0, &W1_L, &W1_C),
        1237 => (1, &W2_L, &W2_C),
        1238 => (2, &W3_L, &W3_C),
        1241 => (0, &W4_L, &W4_C),
        1242 => (1, &W5_L, &W5_C),
        1243 => (2, &W6_L, &W6_C),
        1244 => (1, &W7_L, &W7_C),
        1250 => (3, &W8_L, &W8_C),
        1251 => (4, &W9_L, &W9_C),
        1252 => (5, &W10_L, &W10_C),
        1253 => (1, &W2_L, &W2_C),
        1256 => (0, &W11_L, &W11_C),
        1258 => (5, &W10_L, &W10_C),
        1259 => (1, &W2_L, &W2_C),
        1270 => (0, &W11_L, &W11_C),
        1271 => (0, &W4_L, &W4_C),
        1272 => (2, &W3_L, &W3_C),
        1273 => (1, &W2_L, &W2_C),
        1274 => (1, &W2_L, &W2_C),
        _ => return None,
    };
    let p = if matches!(cid, 1235 | 1241 | 1250) { 8 } else { 32 };
    Some(Cid { set, weights_luma: wl, weights_chroma: wc, p })
}

/// What a coding unit's header says.
#[derive(Debug, Clone)]
pub struct Header {
    pub cid: u32,
    /// Picture width and height (the whole frame, both fields of an interlaced one).
    pub width: u32,
    pub height: u32,
    pub bits: u8,
    pub chroma_444: bool,
    /// The colour format flag (CLF): 1 means RGB, in which a 4:4:4 macroblock is RGB unless its ACF bit says YCbCr.
    pub rgb_format: bool,
    /// Interlaced: the frame is two coding units, one per field.
    pub interlaced: bool,
    /// Macroblock rows in this coding unit (a field's for an interlaced frame).
    pub mb_rows: usize,
    /// H.273 colour code points.
    pub colour_primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
    /// Header size: where the compressed rows start.
    pub header_size: usize,
}

fn be16(b: &[u8], i: usize) -> usize {
    u16::from_be_bytes([b[i], b[i + 1]]) as usize
}
fn be32(b: &[u8], i: usize) -> usize {
    u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]) as usize
}

/// Reads the header of the coding unit that starts `unit`.
pub fn parse_header(unit: &[u8]) -> Result<Header> {
    if unit.len() < 0x280 {
        return Err(Error::Invalid("DNx header cut short"));
    }
    // 0x000 header size (4 bytes), 0x004 header version; the fixed prefix of every version
    if unit[0] != 0 || unit[1] != 0 || !(1..=3).contains(&unit[4]) {
        return Err(Error::Invalid("not a DNxHD / DNxHR frame"));
    }
    let hvn = unit[4];
    let header_size = if hvn == 3 { be32(unit, 0) } else { 0x280 };
    if header_size < 0x280 || header_size > unit.len() {
        return Err(Error::Invalid("DNx header size"));
    }
    let alpf = be16(unit, 0x18);
    let spl = be16(unit, 0x1A);
    let bits = match unit[0x21] >> 5 {
        1 => 8,
        2 => 10,
        3 => 12,
        _ => return Err(Error::Invalid("DNx sample depth")),
    };
    let sst_interlaced = unit[0x22] & 4 != 0;
    let cid = be32(unit, 0x28) as u32;
    let ccb = unit[0x2C];
    let ssc = (ccb >> 5) & 3;
    let clf = ccb & 1;
    let clv = (ccb >> 1) & 3;
    let ns = be16(unit, 0x16C);
    if alpf == 0 || spl == 0 || alpf > 16384 || spl > 16384 || ns == 0 {
        return Err(Error::Invalid("DNx picture size"));
    }
    let chroma_444 = match ssc {
        0 => false,
        2 => true,
        _ => return Err(Error::Unsupported("DNx 4:2:0")),
    };
    if cid == 1260 || cid_params(cid).is_none() {
        return Err(Error::Unsupported("DNx compression ID"));
    }
    if header_size < 0x170 + 4 * ns {
        return Err(Error::Invalid("DNx scan index table"));
    }
    // A field of an interlaced frame holds half the lines
    let height = if sst_interlaced && alpf.div_ceil(16) == ns { alpf * 2 } else { alpf };
    let (primaries, transfer, matrix) = match clv {
        0 => (1, 1, 1),
        1 | 2 => (9, 14, 9),
        _ => (2, 2, 2),
    };
    Ok(Header { cid, width: spl as u32, height: height as u32, bits, chroma_444, rgb_format: clf != 0, interlaced: sst_interlaced, mb_rows: ns, colour_primaries: primaries, transfer, matrix, header_size })
}

// ---------------------------------------------------------------------------------------------
// bits and codeword lookup

struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    /// The next 32 bits or more, left aligned (zeros past the end).
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
    fn get(&mut self, n: u32) -> u32 {
        if n == 0 {
            return 0;
        }
        let v = (self.window() >> (64 - n)) as u32;
        self.pos += n as usize;
        v
    }

    #[inline(always)]
    fn overrun(&self) -> bool {
        self.pos > self.d.len() * 8
    }
}

/// A codeword table: every bit pattern of `bits` bits maps to `len | value << 5` (0 = no code).
struct Lut {
    bits: u32,
    t: Vec<u16>,
}

impl Lut {
    fn new(codes: impl Iterator<Item = (u16, u8, u16)>) -> Lut {
        let codes: Vec<_> = codes.collect();
        let bits = codes.iter().map(|c| c.1 as u32).max().unwrap_or(1);
        let mut t = vec![0u16; 1 << bits];
        for (code, len, val) in codes {
            let lo = (code as usize) << (bits - len as u32);
            let n = 1usize << (bits - len as u32);
            for e in &mut t[lo..lo + n] {
                *e = len as u16 | (val << 5);
            }
        }
        Lut { bits, t }
    }

    #[inline(always)]
    fn read(&self, br: &mut Bits) -> Result<u16> {
        let idx = (br.window() >> (64 - self.bits)) as usize;
        let e = self.t[idx];
        if e == 0 {
            return Err(Error::Invalid("DNx invalid codeword"));
        }
        br.pos += (e & 31) as usize;
        Ok(e >> 5)
    }
}

struct Tables {
    ac: Lut,
    run: Lut,
    dc: Lut,
}

fn tables(set: usize) -> &'static Tables {
    use std::sync::OnceLock;
    static T: [OnceLock<Tables>; 6] = [const { OnceLock::new() }; 6];
    T[set].get_or_init(|| {
        let (ac, run, dc): (&[AcCode; 257], &[(u16, u8, u8); 62], &[(u16, u8, u8)]) = match set {
            0 => (&AC_A, &RUN_A, &DC_A),
            1 => (&AC_B, &RUN_B, &DC_B),
            2 => (&AC_C, &RUN_C, &DC_C),
            3 => (&AC_D, &RUN_D, &DC_D),
            4 => (&AC_E, &RUN_E, &DC_E),
            _ => (&AC_F, &RUN_F, &DC_F),
        };
        Tables {
            // value: amplitude (7 bits) | run flag << 7 | index flag << 8 | end of block << 9
            ac: Lut::new(ac.iter().map(|&(c, l, a, f)| (c, l, a as u16 | ((f as u16) << 7)))),
            run: Lut::new(run.iter().map(|&(c, l, v)| (c, l, v as u16))),
            dc: Lut::new(dc.iter().map(|&(c, l, v)| (c, l, v as u16))),
        }
    })
}

// ---------------------------------------------------------------------------------------------
// macroblock rows

#[derive(Clone, Copy)]
struct PlanePtr {
    p: *mut u16,
    stride: usize,
}
// SAFETY: rows write disjoint macroblocks.
unsafe impl Send for PlanePtr {}
unsafe impl Sync for PlanePtr {}

struct Ctx<'a> {
    frame: &'a [u8],
    hdr: &'a Header,
    cid: &'a Cid,
    tables: &'static Tables,
    planes: [PlanePtr; 3],
    mb_w: usize,
}

#[derive(Clone, Copy)]
struct Job {
    /// Where the row's data starts and ends in the frame.
    at: usize,
    end: usize,
    mb_y: usize,
    /// Interlaced pictures: 0 for the first field (even lines), 1 for the second.
    parity: usize,
}

/// Reads one block into `blk` (dequantised, in raster order).
#[inline(always)]
fn read_block(br: &mut Bits, t: &Tables, dc_pred: &mut i32, weights: &[u8; 64], qsf: i64, p: i64, bits: u8, blk: &mut [i32; 64]) -> Result<()> {
    *blk = [0; 64];
    let n = t.dc.read(br)? as u32;
    let delta = if n == 0 {
        0
    } else {
        let v = br.get(n) as i32;
        if v >= 1 << (n - 1) { v } else { v + 1 - (1 << n) }
    };
    *dc_pred = dc_pred.wrapping_add(delta);
    blk[0] = *dc_pred;
    let index_bits = if bits == 8 { 4 } else { 6 };
    let mut r = 1usize;
    loop {
        let e = t.ac.read(br)?;
        if e & 0x200 != 0 {
            break;
        }
        let mut a = (e & 0x7F) as i64;
        let neg = br.get(1) != 0;
        if e & 0x100 != 0 {
            a += (br.get(index_bits) as i64) * 64;
        }
        if e & 0x80 != 0 {
            r += t.run.read(br)? as usize;
        }
        if r >= 64 {
            return Err(Error::Invalid("DNx coefficient position past the block"));
        }
        let pos = SCAN[r] as usize;
        let w = weights[pos] as i64;
        let wq = w * qsf;
        let mag = ((4 * a + 3) * wq + 2 * p) / (4 * p);
        blk[pos] = if neg { -mag as i32 } else { mag as i32 };
        r += 1;
        if br.overrun() {
            return Err(Error::Invalid("DNx row cut short"));
        }
    }
    Ok(())
}

fn decode_row(c: &Ctx, j: &Job) -> Result<()> {
    let data = c.frame.get(j.at..j.end).ok_or(Error::Invalid("DNx row outside the frame"))?;
    let mut br = Bits { d: data, pos: 0 };
    let bits = c.hdr.bits;
    let mid = 1i32 << (bits - 1);
    let max = (1i32 << bits) - 1;
    // a 12 bit picture's coefficients are a quarter of the scale the other depths use (Table 14)
    let idct_bits = if bits == 12 { 14 } else { 12 };
    let c444 = c.hdr.chroma_444;
    let nblocks = if c444 { 12 } else { 8 };
    let mut dc = [0i32; 3];
    let mut blk = [0i32; 64];
    for mb in 0..c.mb_w {
        let h = br.get(12) as i64;
        let qsf = h >> 1;
        if c444 && c.hdr.rgb_format && h & 1 == 0 {
            return Err(Error::Unsupported("DNx RGB macroblocks"));
        }
        for k in 0..nblocks {
            // component and the block's place in the macroblock (in samples of its plane)
            let (comp, dx, dy) = if c444 {
                let kk = k % 6;
                (kk / 2, (kk & 1) * 8, (k / 6) * 8)
            } else {
                match k {
                    0 => (0, 0, 0),
                    1 => (0, 8, 0),
                    2 => (1, 0, 0),
                    3 => (2, 0, 0),
                    4 => (0, 0, 8),
                    5 => (0, 8, 8),
                    6 => (1, 0, 8),
                    _ => (2, 0, 8),
                }
            };
            let weights = if comp == 0 || c444 { c.cid.weights_luma } else { c.cid.weights_chroma };
            read_block(&mut br, c.tables, &mut dc[comp], weights, qsf, c.cid.p, bits, &mut blk)?;
            idct(&mut blk, idct_bits);
            let mbw_px = if comp == 0 || c444 { 16 } else { 8 };
            let x0 = mb * mbw_px + dx;
            let y0 = j.mb_y * 16 + dy;
            let pp = c.planes[comp];
            for r in 0..8 {
                let y = y0 + r;
                let row = if c.hdr.interlaced { 2 * y + j.parity } else { y };
                // SAFETY: this macroblock belongs to this row alone and lies inside the plane
                unsafe {
                    let dst = pp.p.add(row * pp.stride + x0);
                    for x in 0..8 {
                        *dst.add(x) = (blk[r * 8 + x] + mid).clamp(0, max) as u16;
                    }
                }
            }
        }
        if br.overrun() {
            return Err(Error::Invalid("DNx row cut short"));
        }
    }
    Ok(())
}

/// Decodes DNxHD / DNxHR frames. Macroblock rows run on a thread pool of the size given at creation.
pub struct DnxDecoder {
    pool: Option<rayon::ThreadPool>,
    threads: usize,
}

impl DnxDecoder {
    /// `threads` of 1 decodes on the calling thread; 0 uses every logical core.
    pub fn new(threads: usize) -> Self {
        let threads = if threads == 0 { std::thread::available_parallelism().map_or(4, |n| n.get()) } else { threads };
        let pool = if threads > 1 { rayon::ThreadPoolBuilder::new().num_threads(threads).thread_name(|i| format!("dnx-{i}")).build().ok() } else { None };
        DnxDecoder { pool, threads }
    }

    pub fn threads(&self) -> usize {
        if self.pool.is_some() { self.threads } else { 1 }
    }

    /// Decodes `frame` (one sample of the MOV, both fields of an interlaced one) into `out`.
    pub fn decode(&self, frame: &[u8], out: &mut Picture) -> Result<Header> {
        let hdr = parse_header(frame)?;
        let cid = cid_params(hdr.cid).ok_or(Error::Unsupported("DNx compression ID"))?;
        let mb_w = (hdr.width as usize).div_ceil(16);
        let fields = if hdr.interlaced { 2 } else { 1 };
        let rows = hdr.mb_rows * 16 * fields;
        out.width = hdr.width;
        out.height = hdr.height;
        out.bits = hdr.bits;
        out.chroma_444 = hdr.chroma_444;
        out.rows = rows;
        out.stride_y = mb_w * 16;
        out.stride_c = if hdr.chroma_444 { mb_w * 16 } else { mb_w * 8 };
        out.y.resize(out.stride_y * rows, 0);
        out.u.resize(out.stride_c * rows, 0);
        out.v.resize(out.stride_c * rows, 0);
        if (hdr.height as usize) > rows {
            return Err(Error::Invalid("DNx picture taller than its macroblock rows"));
        }

        let mut jobs: Vec<Job> = Vec::new();
        let unit_len = frame.len() / fields;
        for f in 0..fields {
            let start = f * unit_len;
            let unit = &frame[start..frame.len().min(start + unit_len)];
            let uh = if f == 0 { hdr.clone() } else { parse_header(unit)? };
            if uh.mb_rows != hdr.mb_rows || uh.header_size != hdr.header_size {
                return Err(Error::Invalid("DNx coding units disagree"));
            }
            let mut idx = Vec::with_capacity(hdr.mb_rows + 1);
            for i in 0..hdr.mb_rows {
                idx.push(uh.header_size + be32(unit, 0x170 + 4 * i));
            }
            idx.push(unit.len());
            for i in 0..hdr.mb_rows {
                if idx[i] > idx[i + 1] || idx[i + 1] > unit.len() {
                    return Err(Error::Invalid("DNx scan index table"));
                }
                jobs.push(Job { at: start + idx[i], end: start + idx[i + 1], mb_y: i, parity: f });
            }
        }
        let ctx = Ctx {
            frame,
            hdr: &hdr,
            cid: &cid,
            tables: tables(cid.set),
            planes: [
                PlanePtr { p: out.y.as_mut_ptr(), stride: out.stride_y },
                PlanePtr { p: out.u.as_mut_ptr(), stride: out.stride_c },
                PlanePtr { p: out.v.as_mut_ptr(), stride: out.stride_c },
            ],
            mb_w,
        };
        let result: Result<()> = match &self.pool {
            Some(pool) => {
                use rayon::prelude::*;
                pool.install(|| jobs.par_iter().try_for_each(|j| decode_row(&ctx, j)))
            }
            None => jobs.iter().try_for_each(|j| decode_row(&ctx, j)),
        };
        result?;
        Ok(hdr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_is_a_permutation() {
        let mut seen = [false; 64];
        for &p in SCAN.iter() {
            assert!(!seen[p as usize]);
            seen[p as usize] = true;
        }
        assert_eq!(SCAN[1], 1);
        assert_eq!(SCAN[2], 8);
    }

    #[test]
    fn code_tables_are_complete() {
        // every table is a full prefix code: the lookup of the all ones pattern finds the longest code
        for set in 0..6 {
            let t = tables(set);
            for l in [&t.ac, &t.run, &t.dc] {
                assert!(l.t.iter().all(|&e| e != 0), "set {set} has a hole");
            }
        }
    }
}
