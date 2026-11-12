//! MPEG-1 / MPEG-2 video headers (ISO/IEC 11172-2, 13818-2) and the picture splitter.
//!
//! Only what the GPU needs is parsed: the sequence header and its extension (size, chroma
//! format, progressive_sequence, quantiser matrices), the GOP header, the picture header and the
//! picture coding extension, and the slice headers (quantiser scale code, where the macroblock
//! data starts). The macroblock layer is the video engine's.
//!
//! The splitter cuts an elementary stream into access units at picture start codes. An access
//! unit starts at the first sequence or GOP header in front of its picture header (else at the
//! picture header) and runs up to the start of the next one.

use crate::{Error, Result};

pub const PICTURE_START: u8 = 0x00;
pub const SEQUENCE_HEADER: u8 = 0xB3;
pub const EXTENSION: u8 = 0xB5;
pub const GROUP_START: u8 = 0xB8;

/// Frame rates by `frame_rate_code`, as (numerator, denominator).
pub const FRAME_RATES: [(u32, u32); 9] = [(0, 1), (24000, 1001), (24, 1), (25, 1), (30000, 1001), (30, 1), (50, 1), (60000, 1001), (60, 1)];

/// Scan order (raster index of each position in zig-zag order).
pub const ZIGZAG: [u8; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20, 13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59, 52, 45, 38, 31, 39,
    46, 53, 60, 61, 54, 47, 55, 62, 63,
];

const DEFAULT_INTRA_RASTER: [u8; 64] = [
    8, 16, 19, 22, 26, 27, 29, 34, 16, 16, 22, 24, 27, 29, 34, 37, 19, 22, 26, 27, 29, 34, 34, 38, 22, 22, 26, 27, 29, 34, 37, 40, 22, 26, 27, 29, 32, 35, 40, 48, 26, 27, 29, 32, 35, 40, 48, 58, 26, 27, 29, 34, 38, 46,
    56, 69, 27, 29, 35, 38, 46, 56, 69, 83,
];

/// The default intra matrix in scan order (the order matrices are sent in).
pub fn default_intra() -> [u8; 64] {
    let mut m = [0u8; 64];
    for (k, v) in m.iter_mut().enumerate() {
        *v = DEFAULT_INTRA_RASTER[ZIGZAG[k] as usize];
    }
    m
}

/// The four quantiser matrices (intra, non-intra, chroma intra, chroma non-intra), in scan order.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Matrices(pub [[u8; 64]; 4]);

impl Default for Matrices {
    fn default() -> Self {
        let i = default_intra();
        Self([i, [16; 64], i, [16; 64]])
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Sequence {
    pub width: u32,
    pub height: u32,
    pub aspect_code: u8,
    pub frame_rate_code: u8,
    pub bit_rate: u32,
    /// An extension followed: MPEG-2 (else MPEG-1).
    pub mpeg2: bool,
    pub profile_level: u8,
    pub progressive: bool,
    /// 1 = 4:2:0, 2 = 4:2:2, 3 = 4:4:4.
    pub chroma_format: u8,
    pub low_delay: bool,
    pub matrices: Matrices,
}

impl Sequence {
    pub fn frame_rate(&self) -> f64 {
        let (n, d) = FRAME_RATES.get(self.frame_rate_code as usize).copied().unwrap_or((0, 1));
        n as f64 / d as f64
    }

    /// Macroblock rows of a frame (rounded up to an even number unless the sequence is
    /// progressive).
    pub fn mb_height(&self) -> u32 {
        if self.progressive { self.height.div_ceil(16) } else { self.height.div_ceil(32) * 2 }
    }

    pub fn mb_width(&self) -> u32 {
        self.width.div_ceil(16)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PicType {
    I,
    P,
    B,
}

#[derive(Clone, Debug)]
pub struct Picture {
    pub temporal_reference: u16,
    pub coding_type: PicType,
    /// f_code[s][t]: s = forward / backward, t = horizontal / vertical.
    pub f_code: [[u8; 2]; 2],
    pub intra_dc_precision: u8,
    /// 1 = top field, 2 = bottom field, 3 = frame.
    pub structure: u8,
    pub top_field_first: bool,
    pub frame_pred_frame_dct: bool,
    pub concealment_motion_vectors: bool,
    pub q_scale_type: bool,
    pub intra_vlc_format: bool,
    pub alternate_scan: bool,
    pub repeat_first_field: bool,
    pub chroma_420_type: bool,
    pub progressive_frame: bool,
}

struct Bits<'a> {
    d: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn new(d: &'a [u8]) -> Self {
        Self { d, pos: 0 }
    }
    fn left(&self) -> usize {
        (self.d.len() * 8).saturating_sub(self.pos)
    }
    fn get(&mut self, n: usize) -> Result<u32> {
        if self.left() < n {
            return Err(Error::Invalid("MPEG video header cut short"));
        }
        let mut v = 0u32;
        for _ in 0..n {
            v = v << 1 | ((self.d[self.pos >> 3] >> (7 - (self.pos & 7))) & 1) as u32;
            self.pos += 1;
        }
        Ok(v)
    }
    fn flag(&mut self) -> Result<bool> {
        Ok(self.get(1)? == 1)
    }
}

/// Start codes in `d`: (position of the `00 00 01`, code byte).
pub fn start_codes(d: &[u8]) -> impl Iterator<Item = (usize, u8)> + '_ {
    let mut i = 0;
    std::iter::from_fn(move || {
        while i + 3 < d.len() {
            if d[i + 2] > 1 {
                i += 3;
            } else if d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1 {
                let r = (i, d[i + 3]);
                i += 3;
                return Some(r);
            } else {
                i += 1;
            }
        }
        None
    })
}

/// Parses a sequence header; `d` starts at the code byte (`0xB3`). `ext` is the data after the
/// sequence extension's code byte, when one follows.
pub fn parse_sequence(d: &[u8], ext: Option<&[u8]>) -> Result<Sequence> {
    let mut b = Bits::new(d.get(1..).ok_or(Error::Invalid("MPEG sequence header cut short"))?);
    let mut width = b.get(12)?;
    let mut height = b.get(12)?;
    let aspect_code = b.get(4)? as u8;
    let frame_rate_code = b.get(4)? as u8;
    let mut bit_rate = b.get(18)?;
    b.get(1)?;
    b.get(10)?;
    b.get(1)?;
    let mut matrices = Matrices::default();
    if b.flag()? {
        for v in matrices.0[0].iter_mut() {
            *v = b.get(8)? as u8;
        }
    }
    if b.flag()? {
        for v in matrices.0[1].iter_mut() {
            *v = b.get(8)? as u8;
        }
    }
    matrices.0[2] = matrices.0[0];
    matrices.0[3] = matrices.0[1];
    let mut s = Sequence { width, height, aspect_code, frame_rate_code, bit_rate: 0, mpeg2: false, profile_level: 0, progressive: true, chroma_format: 1, low_delay: false, matrices };
    if let Some(e) = ext {
        let mut b = Bits::new(e);
        if b.get(4)? == 1 {
            s.mpeg2 = true;
            s.profile_level = b.get(8)? as u8;
            s.progressive = b.flag()?;
            s.chroma_format = b.get(2)? as u8;
            width |= b.get(2)? << 12;
            height |= b.get(2)? << 12;
            bit_rate |= b.get(12)? << 18;
            b.get(1)?;
            b.get(8)?;
            s.low_delay = b.flag()?;
            s.width = width;
            s.height = height;
        }
    }
    s.bit_rate = bit_rate;
    if s.width == 0 || s.height == 0 {
        return Err(Error::Invalid("MPEG video of size zero"));
    }
    Ok(s)
}

/// A quantiser matrix extension (`d` after the code byte, id 3): replaces the matrices it loads.
pub fn parse_quant_ext(d: &[u8], m: &mut Matrices) -> Result<()> {
    let mut b = Bits::new(d);
    if b.get(4)? != 3 {
        return Ok(());
    }
    let mut new = m.clone();
    for k in 0..4 {
        if b.flag()? {
            for v in new.0[k].iter_mut() {
                *v = b.get(8)? as u8;
            }
        }
    }
    *m = new;
    Ok(())
}

/// A picture header (`d` after the code byte); the coding extension is applied by
/// [`apply_coding_ext`].
pub fn parse_picture(d: &[u8]) -> Result<Picture> {
    let mut b = Bits::new(d);
    let temporal_reference = b.get(10)? as u16;
    let coding_type = match b.get(3)? {
        1 => PicType::I,
        2 => PicType::P,
        3 => PicType::B,
        _ => return Err(Error::Unsupported("MPEG picture coding type other than I, P and B (D pictures)")),
    };
    b.get(16)?;
    let mut f_code = [[15u8; 2]; 2];
    if coding_type != PicType::I {
        b.get(1)?;
        let f = b.get(3)? as u8;
        f_code[0] = [f, f];
        if coding_type == PicType::B {
            b.get(1)?;
            let f = b.get(3)? as u8;
            f_code[1] = [f, f];
        }
    }
    Ok(Picture {
        temporal_reference,
        coding_type,
        f_code,
        intra_dc_precision: 0,
        structure: 3,
        top_field_first: false,
        frame_pred_frame_dct: true,
        concealment_motion_vectors: false,
        q_scale_type: false,
        intra_vlc_format: false,
        alternate_scan: false,
        repeat_first_field: false,
        chroma_420_type: true,
        progressive_frame: true,
    })
}

/// A picture coding extension (`d` after the code byte, id 8).
pub fn apply_coding_ext(p: &mut Picture, d: &[u8]) -> Result<()> {
    let mut b = Bits::new(d);
    if b.get(4)? != 8 {
        return Ok(());
    }
    for s in 0..2 {
        for t in 0..2 {
            p.f_code[s][t] = b.get(4)? as u8;
        }
    }
    p.intra_dc_precision = b.get(2)? as u8;
    p.structure = b.get(2)? as u8;
    p.top_field_first = b.flag()?;
    p.frame_pred_frame_dct = b.flag()?;
    p.concealment_motion_vectors = b.flag()?;
    p.q_scale_type = b.flag()?;
    p.intra_vlc_format = b.flag()?;
    p.alternate_scan = b.flag()?;
    p.repeat_first_field = b.flag()?;
    p.chroma_420_type = b.flag()?;
    p.progressive_frame = b.flag()?;
    Ok(())
}

/// `closed_gop` of a GOP header (`d` after the code byte).
pub fn gop_closed(d: &[u8]) -> bool {
    d.len() >= 4 && d[3] & 0x40 != 0
}

/// A slice of a picture: where it starts (at its `00 00 01 xx`) and ends, its row, and the bit
/// offset of its first macroblock from the start code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SliceInfo {
    pub start: usize,
    pub end: usize,
    pub row: u32,
    pub quant_code: u8,
    pub mb_bit_offset: u32,
}

/// The slices of the picture data in `d` (picture header onward), in order.
pub fn slices(d: &[u8], tall: bool) -> Vec<SliceInfo> {
    let codes: Vec<(usize, u8)> = start_codes(d).collect();
    let mut out: Vec<SliceInfo> = Vec::new();
    for (n, &(pos, code)) in codes.iter().enumerate() {
        if code == 0 || code >= 0xB0 {
            if let Some(last) = out.last_mut() {
                if last.end == usize::MAX {
                    last.end = pos;
                }
            }
            continue;
        }
        if let Some(last) = out.last_mut() {
            if last.end == usize::MAX {
                last.end = pos;
            }
        }
        let end = codes.get(n + 1).map_or(d.len(), |c| c.0);
        let body = &d[pos + 4..end.min(d.len())];
        let mut b = Bits::new(body);
        let mut row = code as u32 - 1;
        if tall {
            let Ok(ext) = b.get(3) else { continue };
            row += ext << 7;
        }
        let Ok(q) = b.get(5) else { continue };
        // slice_extension_flag + 8 bits, extra_bit_slice + 8 bits, each while the next bit is 1
        loop {
            match b.get(1) {
                Ok(1) => {
                    if b.get(8).is_err() {
                        break;
                    }
                }
                _ => break,
            }
        }
        out.push(SliceInfo { start: pos, end: usize::MAX, row, quant_code: q as u8, mb_bit_offset: 32 + b.pos as u32 });
    }
    for s in &mut out {
        if s.end == usize::MAX {
            s.end = d.len();
        }
    }
    out
}

/// One access unit found by the [`Splitter`].
#[derive(Clone, Debug)]
pub struct Unit {
    /// Position of the unit's first byte in the elementary stream.
    pub start: u64,
    pub temporal_reference: u16,
    pub coding_type: PicType,
    /// A sequence or GOP header comes before the picture.
    pub has_header: bool,
    /// The presentation / decode time of the PES packet the picture starts in, when it is the
    /// first picture in that packet.
    pub pts: Option<i64>,
    pub dts: Option<i64>,
}

/// Cuts an elementary stream fed in pieces into access units.
#[derive(Default)]
pub struct Splitter {
    /// Bytes of the previous piece that could hold the start of a start code, and their position.
    tail: Vec<u8>,
    tail_pos: u64,
    /// The first sequence / GOP header since the last slice, waiting for its picture.
    pending: Option<u64>,
    pub units: Vec<Unit>,
    /// (first ES byte of a PES, pts, dts, used).
    marks: Vec<(u64, u64, Option<i64>, Option<i64>, bool)>,
    end: u64,
}

const LOOKAHEAD: usize = 5;

impl Splitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds the next piece (`data`, whose first byte is at `pos` in the stream), which comes with
    /// the times of the PES packet it starts, if any.
    pub fn push(&mut self, pos: u64, data: &[u8], pts: Option<i64>, dts: Option<i64>) {
        if pts.is_some() {
            self.marks.push((pos, pos + data.len() as u64, pts, dts, false));
        }
        let mut buf = std::mem::take(&mut self.tail);
        let base = if buf.is_empty() { pos } else { self.tail_pos };
        buf.extend_from_slice(data);
        self.end = pos + data.len() as u64;
        let limit = buf.len().saturating_sub(LOOKAHEAD);
        let keep_from = self.scan(&buf, base, limit);
        self.tail = buf[keep_from..].to_vec();
        self.tail_pos = base + keep_from as u64;
    }

    /// Scans start codes starting before `limit`; returns where the unscanned rest starts.
    fn scan(&mut self, buf: &[u8], base: u64, limit: usize) -> usize {
        for (p, code) in start_codes(buf) {
            if p >= limit {
                break;
            }
            let abs = base + p as u64;
            match code {
                PICTURE_START => {
                    let tr = buf.get(p + 4..p + 6).map_or(0, |b| (b[0] as u16) << 2 | (b[1] >> 6) as u16);
                    let ty = buf.get(p + 5).map_or(0, |b| (b >> 3) & 7);
                    let coding_type = match ty {
                        1 => PicType::I,
                        3 => PicType::B,
                        _ => PicType::P,
                    };
                    let start = self.pending.take();
                    let (pts, dts) = self.take_mark(abs);
                    self.units.push(Unit { start: start.unwrap_or(abs), temporal_reference: tr, coding_type, has_header: start.is_some(), pts, dts });
                }
                SEQUENCE_HEADER | GROUP_START => {
                    if self.pending.is_none() {
                        self.pending = Some(abs);
                    }
                }
                _ => {}
            }
        }
        // start codes at indices >= limit are scanned again from the tail
        limit
    }

    fn take_mark(&mut self, abs: u64) -> (Option<i64>, Option<i64>) {
        // the packet the picture starts in: the last one starting at or before it
        if let Some(m) = self.marks.iter_mut().rev().find(|m| m.0 <= abs && abs < m.1) {
            if !m.4 {
                m.4 = true;
                return (m.2, m.3);
            }
        }
        (None, None)
    }

    /// Ends the stream: the last bytes are scanned. Returns the units with their sizes.
    pub fn finish(&mut self) -> Vec<(Unit, u64)> {
        let buf = std::mem::take(&mut self.tail);
        let base = self.tail_pos;
        let n = buf.len();
        self.scan(&buf, base, n);
        let mut out = Vec::new();
        for (k, u) in self.units.iter().enumerate() {
            let end = self.units.get(k + 1).map_or(self.end, |n| n.start);
            out.push((u.clone(), end - u.start));
        }
        out
    }
}

/// Presentation and decode times (90 kHz) for the units in decode order: those the PES packets
/// gave are kept, the others are counted from the temporal references within the GOP.
pub fn sample_times(units: &[(Unit, u64)], rate_code: u8) -> Vec<(i64, i64)> {
    let dur = FRAME_RATES.get(rate_code as usize).filter(|r| r.0 > 0).map_or(3600, |r| (90000 * r.1 / r.0 as u32) as i64);
    let mut out = Vec::with_capacity(units.len());
    let mut base: Option<i64> = None;
    let mut group_len = 0i64;
    let mut prev_dts: Option<i64> = None;
    for (u, _) in units {
        if u.has_header {
            // a new GOP: its temporal references count from 0 again
            base = base.map(|b| b + group_len * dur);
            group_len = 0;
        }
        group_len += 1;
        let tr = u.temporal_reference as i64 * dur;
        if let Some(p) = u.pts {
            base = Some(p - tr);
        }
        let pts = u.pts.unwrap_or_else(|| base.unwrap_or(0) + tr);
        base.get_or_insert(pts - tr);
        let dts = u.dts.unwrap_or_else(|| prev_dts.map_or(pts - dur, |d| d + dur)).min(pts);
        prev_dts = Some(dts);
        out.push((pts, dts));
    }
    out
}

pub fn is_elementary(head: &[u8]) -> bool {
    head.len() >= 4 && head[..4] == [0, 0, 1, SEQUENCE_HEADER]
}

/// A raw MPEG-1 / MPEG-2 video elementary stream (`.m2v`): one video track whose samples are
/// the pictures, read straight from the file.
pub fn read_elementary<R: std::io::Read + std::io::Seek>(r: &mut R) -> Result<crate::mp4::Movie> {
    use crate::mp4::{Codec, Kind, Sample, Storage, Track};
    r.seek(std::io::SeekFrom::Start(0))?;
    let mut sp = Splitter::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut pos = 0u64;
    let mut first = Vec::new();
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        if first.len() < 4096 {
            first.extend_from_slice(&buf[..n.min(4096)]);
        }
        sp.push(pos, &buf[..n], None, None);
        pos += n as u64;
    }
    let units = sp.finish();
    if units.is_empty() {
        return Err(Error::Invalid("MPEG video stream without pictures"));
    }
    let codes: Vec<(usize, u8)> = start_codes(&first).collect();
    let seq = codes.iter().position(|c| c.1 == SEQUENCE_HEADER).and_then(|n| {
        let p = codes[n].0;
        let end = codes.get(n + 1).map_or(first.len(), |x| x.0);
        let ext = codes.get(n + 1).filter(|x| x.1 == EXTENSION).map(|x| &first[x.0 + 4..codes.get(n + 2).map_or(first.len(), |y| y.0)]);
        parse_sequence(&first[p + 3..end], ext).ok()
    });
    let seq = seq.ok_or(Error::Invalid("no MPEG sequence header"))?;
    let times = sample_times(&units, seq.frame_rate_code);
    let samples: Vec<Sample> = units.iter().zip(times).map(|((u, size), (pts, dts))| Sample { offset: u.start, size: *size as u32, dts, pts, key: u.coding_type == PicType::I }).collect();
    let track = Track {
        meta: Default::default(),
        id: 1,
        kind: Kind::Video,
        fourcc: if seq.mpeg2 { *b"mpg2" } else { *b"mpg1" },
        codec: Codec::Other,
        timescale: 90000,
        width: seq.width,
        height: seq.height,
        rotation: 0,
        channels: 0,
        sample_rate: 0,
        samples,
        colour: None,
        default: true,
        codec_delay_ns: 0,
        discard_padding_ns: 0,
        par: None,
        storage: Storage::Plain,
    };
    Ok(crate::ts::finish_movie(vec![track]))
}

/// The access units in a block of an elementary stream (one container sample may hold several
/// pictures, e.g. two field pictures): (start, end) byte ranges, each ending where the next
/// begins.
pub fn split_units(d: &[u8]) -> Vec<(usize, usize)> {
    let mut s = Splitter::new();
    s.push(0, d, None, None);
    let v = s.finish();
    v.iter().map(|(u, n)| (u.start as usize, (u.start + n) as usize)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sc(code: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![0, 0, 1, code];
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn default_matrix_is_in_scan_order() {
        let m = default_intra();
        assert_eq!(&m[..8], &[8, 16, 16, 19, 16, 19, 22, 22]);
        assert_eq!(m[63], 83);
    }

    #[test]
    fn sequence_header_with_extension() {
        // 720x576, 4:3, 25 fps, bit rate 0x3ffff, no matrices
        let mut d = vec![0xB3, 0x2D, 0x02, 0x40, 0x23, 0xFF, 0xFF, 0xE0, 0x00, 0x00];
        d.truncate(10);
        // extension: id 1, profile 4 level 8 (0x48), progressive 0, chroma 1
        let ext = [0x14, 0x82, 0x00, 0x01, 0x00, 0x00];
        let s = parse_sequence(&d, Some(&ext)).unwrap();
        assert_eq!((s.width, s.height), (720, 576));
        assert_eq!(s.frame_rate(), 25.0);
        assert!(s.mpeg2 && !s.progressive);
        assert_eq!(s.chroma_format, 1);
        assert_eq!(s.profile_level, 0x48);
        assert_eq!(s.mb_height(), 36);
        assert_eq!(s.matrices, Matrices::default());
    }

    #[test]
    fn picture_header_and_extension() {
        // temporal_reference 5, B picture, vbv 0xffff, forward f 3, backward f 2
        let mut w = Writer::default();
        w.put(5, 10);
        w.put(3, 3);
        w.put(0xFFFF, 16);
        w.put(0, 1);
        w.put(3, 3);
        w.put(0, 1);
        w.put(2, 3);
        let p = parse_picture(&w.bytes()).unwrap();
        assert_eq!(p.temporal_reference, 5);
        assert_eq!(p.coding_type, PicType::B);
        assert_eq!(p.f_code, [[3, 3], [2, 2]]);
        let mut w = Writer::default();
        w.put(8, 4);
        for f in [3, 2, 15, 15] {
            w.put(f, 4);
        }
        w.put(2, 2); // dc precision
        w.put(3, 2); // frame
        w.put(1, 1); // tff
        w.put(0, 1);
        w.put(0, 1);
        w.put(1, 1); // q scale type
        w.put(1, 1); // intra vlc
        w.put(1, 1); // alternate scan
        w.put(0, 1);
        w.put(1, 1);
        w.put(0, 1);
        let mut p = p;
        apply_coding_ext(&mut p, &w.bytes()).unwrap();
        assert_eq!(p.f_code, [[3, 2], [15, 15]]);
        assert_eq!(p.intra_dc_precision, 2);
        assert!(p.top_field_first && p.q_scale_type && p.intra_vlc_format && p.alternate_scan);
        assert!(!p.frame_pred_frame_dct && !p.progressive_frame);
    }

    #[derive(Default)]
    struct Writer {
        v: Vec<u8>,
        n: usize,
    }

    impl Writer {
        fn put(&mut self, val: u32, bits: usize) {
            for i in (0..bits).rev() {
                if self.n % 8 == 0 {
                    self.v.push(0);
                }
                let bit = ((val >> i) & 1) as u8;
                *self.v.last_mut().unwrap() |= bit << (7 - self.n % 8);
                self.n += 1;
            }
        }
        fn bytes(&self) -> Vec<u8> {
            self.v.clone()
        }
    }

    #[test]
    fn slice_headers() {
        // slice 3 (row 2), quantiser code 9, no extension: 5 bits 01001, extra bit 0, then data
        let mut w = Writer::default();
        w.put(9, 5);
        w.put(0, 1);
        w.put(0xFF, 8);
        let mut d = sc(PICTURE_START, &[0, 0, 0, 0]);
        d.extend(sc(3, &w.bytes()));
        d.extend(sc(4, &w.bytes()));
        let s = slices(&d, false);
        assert_eq!(s.len(), 2);
        assert_eq!((s[0].row, s[0].quant_code, s[0].mb_bit_offset), (2, 9, 32 + 6));
        assert_eq!(s[0].end, s[1].start);
        assert_eq!(s[1].row, 3);
    }

    #[test]
    fn splitter_cuts_before_headers_and_across_pieces() {
        let mut es = Vec::new();
        es.extend(sc(SEQUENCE_HEADER, &[0; 8]));
        es.extend(sc(GROUP_START, &[0; 4]));
        es.extend(sc(PICTURE_START, &[0x00, 0x08, 0xFF, 0xF8])); // tr 0, I
        es.extend(sc(1, &[0xAA; 20]));
        es.extend(sc(PICTURE_START, &[0x00, 0xD0, 0xFF, 0xF8])); // tr 3, P (type 2)
        es.extend(sc(1, &[0xBB; 30]));
        es.extend(sc(PICTURE_START, &[0x00, 0x18, 0xFF, 0xF8])); // tr 1, B (type 3)
        es.extend(sc(1, &[0xCC; 7]));
        for piece in [1usize, 2, 3, 5, 7, 4096] {
            let mut s = Splitter::new();
            let mut pos = 0usize;
            while pos < es.len() {
                let n = piece.min(es.len() - pos);
                s.push(pos as u64, &es[pos..pos + n], if pos == 0 { Some(90) } else { None }, None);
                pos += n;
            }
            let u = s.finish();
            assert_eq!(u.len(), 3, "piece {piece}");
            assert_eq!(u[0].0.start, 0);
            assert!(u[0].0.has_header && !u[1].0.has_header);
            if piece == 4096 {
                assert_eq!(u[0].0.pts, Some(90));
            }
            assert_eq!(u[1].0.start, 12 + 8 + 8 + 24);
            assert_eq!((u[0].0.coding_type, u[1].0.coding_type, u[2].0.coding_type), (PicType::I, PicType::P, PicType::B));
            assert_eq!(u[1].0.temporal_reference, 3);
            assert_eq!(u.iter().map(|x| x.1).sum::<u64>(), es.len() as u64);
        }
    }
}
