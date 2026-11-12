//! Bit reading for H.264 / HEVC syntax: fixed-width fields, Exp-Golomb codes, and an optional
//! trace of every element read (named as the standards and ffmpeg's `trace_headers` name them,
//! so tests can compare the two field by field).

use crate::{Error, Result};

pub struct BitReader<'a> {
    data: &'a [u8],
    /// Position in bits.
    pos: usize,
    trace: Option<Vec<(String, i64)>>,
}

impl<'a> BitReader<'a> {
    /// Reads `rbsp` (emulation prevention already removed: see [`crate::nal::unescape`]).
    pub fn new(rbsp: &'a [u8]) -> Self {
        Self { data: rbsp, pos: 0, trace: None }
    }

    /// Like [`BitReader::new`], recording every named element.
    pub fn traced(rbsp: &'a [u8]) -> Self {
        Self { data: rbsp, pos: 0, trace: Some(Vec::new()) }
    }

    pub fn take_trace(&mut self) -> Vec<(String, i64)> {
        self.trace.take().unwrap_or_default()
    }

    pub fn tracing(&self) -> bool {
        self.trace.is_some()
    }

    pub fn note(&mut self, name: String, v: i64) {
        if let Some(t) = &mut self.trace {
            t.push((name, v));
        }
    }

    pub fn bits_left(&self) -> usize {
        (self.data.len() * 8).saturating_sub(self.pos)
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn byte_aligned(&self) -> bool {
        self.pos % 8 == 0
    }

    /// `n` bits (at most 32), most significant first.
    pub fn bits(&mut self, n: u32) -> Result<u32> {
        debug_assert!(n <= 32);
        if n == 0 {
            return Ok(0);
        }
        if self.bits_left() < n as usize {
            return Err(Error::Invalid("bitstream cut short"));
        }
        let mut v = 0u64;
        let mut need = n as usize;
        while need > 0 {
            let byte = self.data[self.pos / 8];
            let off = self.pos % 8;
            let take = (8 - off).min(need);
            let chunk = (byte >> (8 - off - take)) & ((1u16 << take) - 1) as u8;
            v = (v << take) | chunk as u64;
            self.pos += take;
            need -= take;
        }
        Ok(v as u32)
    }

    pub fn skip(&mut self, n: usize) -> Result<()> {
        if self.bits_left() < n {
            return Err(Error::Invalid("bitstream cut short"));
        }
        self.pos += n;
        Ok(())
    }

    pub fn flag(&mut self) -> Result<bool> {
        Ok(self.bits(1)? == 1)
    }

    /// Unsigned Exp-Golomb.
    pub fn ue(&mut self) -> Result<u32> {
        let mut zeros = 0u32;
        while !self.flag()? {
            zeros += 1;
            if zeros > 31 {
                return Err(Error::Invalid("Exp-Golomb code too long"));
            }
        }
        if zeros == 0 {
            return Ok(0);
        }
        let rest = self.bits(zeros)? as u64;
        Ok(((1u64 << zeros) - 1 + rest) as u32)
    }

    /// Signed Exp-Golomb.
    pub fn se(&mut self) -> Result<i32> {
        let k = self.ue()? as i64;
        Ok(if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) } as i32)
    }

    /// Whether syntax data follows (anything before the stop bit and its zero padding).
    pub fn more_rbsp_data(&self) -> bool {
        let total = self.data.len() * 8;
        if self.pos >= total {
            return false;
        }
        // the last set bit in the data is the stop bit
        let Some(last) = self.data.iter().rposition(|&b| b != 0) else { return false };
        let stop = last * 8 + 7 - self.data[last].trailing_zeros() as usize;
        self.pos < stop
    }
}

/// Reads one element and records it under its name: `r!(reader, u 4, "x")`, `r!(reader, ue,
/// "y[{}]", i)`, `r!(reader, se, "z")`, `r!(reader, flag, "f")`. The name is only formatted
/// while tracing.
#[macro_export]
macro_rules! r {
    ($b:expr, u $n:expr, $($name:tt)+) => {{
        let v = $b.bits($n)?;
        if $b.tracing() { $b.note(format!($($name)+), v as i64); }
        v
    }};
    ($b:expr, ue, $($name:tt)+) => {{
        let v = $b.ue()?;
        if $b.tracing() { $b.note(format!($($name)+), v as i64); }
        v
    }};
    ($b:expr, se, $($name:tt)+) => {{
        let v = $b.se()?;
        if $b.tracing() { $b.note(format!($($name)+), v as i64); }
        v
    }};
    ($b:expr, flag, $($name:tt)+) => {{
        let v = $b.flag()?;
        if $b.tracing() { $b.note(format!($($name)+), v as i64); }
        v
    }};
}

#[cfg(test)]
mod tests {
    use super::BitReader;

    #[test]
    fn fields_and_exp_golomb() {
        // 101 | 1 (ue 0) | 010 (ue 1) | 011 (ue 2) | 00100 (ue 3) | 00101 (se -2)  + padding
        let data = [0b1011_0100, 0b1100_1000, 0b0101_0000];
        let mut b = BitReader::new(&data);
        assert_eq!(b.bits(3).unwrap(), 0b101);
        assert_eq!(b.ue().unwrap(), 0);
        assert_eq!(b.ue().unwrap(), 1);
        assert_eq!(b.ue().unwrap(), 2);
        assert_eq!(b.ue().unwrap(), 3);
        assert_eq!(b.se().unwrap(), -2);
        assert!(b.bits(8).is_err());
    }

    #[test]
    fn wide_fields_cross_bytes() {
        let data = [0x12, 0x34, 0x56, 0x78, 0x9a];
        let mut b = BitReader::new(&data);
        b.skip(4).unwrap();
        assert_eq!(b.bits(32).unwrap(), 0x2345_6789);
    }

    #[test]
    fn more_data_stops_at_the_stop_bit() {
        // one data bit, then the stop bit and zeros
        let data = [0b1100_0000];
        let mut b = BitReader::new(&data);
        assert!(b.more_rbsp_data());
        b.flag().unwrap();
        assert!(!b.more_rbsp_data());
    }
}
