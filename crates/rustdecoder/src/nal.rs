//! NAL units: the length-prefixed form MP4 stores them in, the decoder configuration records
//! (`avcC`, `hvcC`) that carry the parameter sets, and emulation prevention.

use crate::{Error, Result};

/// Removes emulation prevention bytes (`00 00 03` → `00 00`), giving the raw payload.
pub fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// The NAL units of one sample, each prefixed by its length in `len_size` bytes (1, 2 or 4).
pub fn units(sample: &[u8], len_size: usize) -> impl Iterator<Item = Result<&[u8]>> {
    let mut p = 0usize;
    std::iter::from_fn(move || {
        if p >= sample.len() {
            return None;
        }
        if p + len_size > sample.len() {
            p = sample.len();
            return Some(Err(Error::Invalid("NAL length cut short")));
        }
        let n = sample[p..p + len_size].iter().fold(0usize, |a, &b| (a << 8) | b as usize);
        p += len_size;
        if n > sample.len() - p {
            p = sample.len();
            return Some(Err(Error::Invalid("NAL unit runs past its sample")));
        }
        let u = &sample[p..p + n];
        p += n;
        Some(Ok(u))
    })
}

/// Index of the next `00 00 01` at or after `from`.
fn next_start_code(d: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 2 < d.len() {
        // a start code needs a 1 as its third byte, so anything above 1 rules out three positions
        if d[i + 2] > 1 {
            i += 3;
        } else if d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1 {
            return Some(i);
        } else {
            i += 1;
        }
    }
    None
}

/// The NAL units of an Annex B byte stream (start codes `00 00 01` / `00 00 00 01`), without the
/// start codes and without the zero bytes that may trail a unit. Bytes before the first start
/// code are ignored.
pub fn annexb_units(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut at = next_start_code(data, 0);
    std::iter::from_fn(move || {
        let start = at? + 3;
        at = next_start_code(data, start);
        let mut end = at.unwrap_or(data.len());
        while end > start && data[end - 1] == 0 {
            end -= 1;
        }
        Some(&data[start..end])
    })
}

/// Units the decoders have no use for: access unit delimiters and filler data.
pub fn annexb_dropped(unit: &[u8], hevc: bool) -> bool {
    match unit.first() {
        None => true,
        Some(b) if hevc => matches!((b >> 1) & 0x3f, 35 | 38),
        Some(b) => matches!(b & 0x1f, 9 | 12),
    }
}

/// Appends the NAL units of an Annex B access unit to `out`, each behind a 4-byte length (the
/// form `avcC` / `hvcC` with a length size of 4 describes). Access unit delimiters and filler
/// data are left out.
pub fn annexb_to_length_prefixed(src: &[u8], hevc: bool, out: &mut Vec<u8>) {
    for u in annexb_units(src) {
        if annexb_dropped(u, hevc) {
            continue;
        }
        out.extend_from_slice(&(u.len() as u32).to_be_bytes());
        out.extend_from_slice(u);
    }
}

/// What a decoder configuration record holds.
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// Bytes in each NAL unit's length prefix.
    pub len_size: usize,
    /// Parameter sets (and any SEI) in the order stored, escaped as in the stream.
    pub units: Vec<Vec<u8>>,
}

/// The `hvcC` record (ISO/IEC 14496-15, 8.3.3).
pub fn hvcc(b: &[u8]) -> Result<Config> {
    if b.len() < 23 {
        return Err(Error::Invalid("hvcC too short"));
    }
    let len_size = (b[21] & 3) as usize + 1;
    let arrays = b[22];
    let mut p = 23usize;
    let mut units = Vec::new();
    for _ in 0..arrays {
        if p + 3 > b.len() {
            return Err(Error::Invalid("hvcC array cut short"));
        }
        let n = u16::from_be_bytes([b[p + 1], b[p + 2]]);
        p += 3;
        for _ in 0..n {
            if p + 2 > b.len() {
                return Err(Error::Invalid("hvcC unit cut short"));
            }
            let len = u16::from_be_bytes([b[p], b[p + 1]]) as usize;
            p += 2;
            let u = b.get(p..p + len).ok_or(Error::Invalid("hvcC unit cut short"))?;
            units.push(u.to_vec());
            p += len;
        }
    }
    Ok(Config { len_size, units })
}

/// The `avcC` record (ISO/IEC 14496-15, 5.3.3).
pub fn avcc(b: &[u8]) -> Result<Config> {
    if b.len() < 7 {
        return Err(Error::Invalid("avcC too short"));
    }
    let len_size = (b[4] & 3) as usize + 1;
    let mut p = 5usize;
    let mut units = Vec::new();
    for group in 0..2 {
        let n = if group == 0 { b[p] & 0x1f } else { *b.get(p).ok_or(Error::Invalid("avcC cut short"))? };
        p += 1;
        for _ in 0..n {
            if p + 2 > b.len() {
                return Err(Error::Invalid("avcC unit cut short"));
            }
            let len = u16::from_be_bytes([b[p], b[p + 1]]) as usize;
            p += 2;
            let u = b.get(p..p + len).ok_or(Error::Invalid("avcC unit cut short"))?;
            units.push(u.to_vec());
            p += len;
        }
    }
    Ok(Config { len_size, units })
}

#[cfg(test)]
mod tests {
    #[test]
    fn unescape_removes_prevention_bytes() {
        assert_eq!(super::unescape(&[0, 0, 3, 1, 0, 0, 3, 0, 0, 3]), [0, 0, 1, 0, 0, 0, 0]);
        // a 03 not after two zeros stays
        assert_eq!(super::unescape(&[0, 3, 0, 0, 2, 3]), [0, 3, 0, 0, 2, 3]);
    }

    #[test]
    fn annexb_to_lengths() {
        // AUD, then a 4-byte start code SPS with trailing zeros, then a 3-byte start code slice
        let s = [0, 0, 0, 1, 9, 0xf0, 0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 0, 1, 0x65, 9, 8, 7];
        let mut o = Vec::new();
        super::annexb_to_length_prefixed(&s, false, &mut o);
        assert_eq!(o, [0, 0, 0, 3, 0x67, 1, 2, 0, 0, 0, 4, 0x65, 9, 8, 7]);
        assert_eq!(super::annexb_units(&[5, 5, 0, 0, 1, 1, 0, 0, 1, 2]).collect::<Vec<_>>(), [&[1u8][..], &[2u8][..]]);
    }

    #[test]
    fn length_prefixed_units() {
        let s = [0, 0, 0, 2, 0xaa, 0xbb, 0, 0, 0, 1, 0xcc];
        let u: Vec<&[u8]> = super::units(&s, 4).map(|u| u.unwrap()).collect();
        assert_eq!(u, [&[0xaa, 0xbb][..], &[0xcc][..]]);
        assert!(super::units(&[0, 0, 0, 9, 1], 4).next().unwrap().is_err());
    }
}
