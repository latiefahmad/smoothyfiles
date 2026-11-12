//! Annex B byte streams (NAL units behind `00 00 01` start codes), as MXF stores H.264 and HEVC,
//! turned into the length-prefixed form the decoders read, plus the decoder configuration records
//! (`avcC`, `hvcC`) made from the parameter sets found in the stream.

/// Offsets of the NAL units in `data`: (start of the unit, end of the unit). Trailing zero bytes
/// (the `zero_byte` of the next 4-byte start code, `trailing_zero_8bits`) are not part of a unit.
pub fn nal_ranges(data: &[u8]) -> Vec<(usize, usize)> {
    let mut starts = Vec::new();
    let mut i = 0usize;
    while i + 3 <= data.len() {
        if data[i + 2] > 1 {
            i += 3;
        } else if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &s) in starts.iter().enumerate() {
        let mut e = if k + 1 < starts.len() { starts[k + 1] - 3 } else { data.len() };
        while e > s && data[e - 1] == 0 {
            e -= 1;
        }
        if e > s {
            out.push((s, e));
        }
    }
    out
}

/// A sample that starts with a 4-byte start code. A length-prefixed sample cannot start with
/// `00 00 00 01` (a NAL unit of one byte), so this tells the two forms apart.
pub fn is_annexb(sample: &[u8]) -> bool {
    sample.len() >= 5 && sample[..4] == [0, 0, 0, 1]
}

/// Appends the NAL units of an Annex B `data` to `out`, each behind a 4-byte length.
pub fn to_length_prefixed(data: &[u8], out: &mut Vec<u8>) {
    for (s, e) in nal_ranges(data) {
        out.extend_from_slice(&((e - s) as u32).to_be_bytes());
        out.extend_from_slice(&data[s..e]);
    }
}

/// H.264 NAL unit type.
pub fn h264_type(unit: &[u8]) -> u8 {
    unit.first().map_or(0, |b| b & 0x1f)
}

/// HEVC NAL unit type.
pub fn hevc_type(unit: &[u8]) -> u8 {
    unit.first().map_or(0, |b| (b >> 1) & 0x3f)
}

/// The `avcC` record for the SPS and PPS units in `data` (Annex B), length size 4. `None` when
/// either is missing.
pub fn avcc_from(data: &[u8]) -> Option<Vec<u8>> {
    let mut sps: Vec<&[u8]> = Vec::new();
    let mut pps: Vec<&[u8]> = Vec::new();
    for (s, e) in nal_ranges(data) {
        let u = &data[s..e];
        match h264_type(u) {
            7 if u.len() >= 4 && !sps.contains(&u) => sps.push(u),
            8 if !pps.contains(&u) => pps.push(u),
            _ => {}
        }
    }
    let first = sps.first()?;
    if pps.is_empty() {
        return None;
    }
    let mut v = vec![1, first[1], first[2], first[3], 0xff, 0xe0 | sps.len().min(31) as u8];
    for u in sps.iter().take(31) {
        v.extend_from_slice(&(u.len() as u16).to_be_bytes());
        v.extend_from_slice(u);
    }
    v.push(pps.len().min(255) as u8);
    for u in pps.iter().take(255) {
        v.extend_from_slice(&(u.len() as u16).to_be_bytes());
        v.extend_from_slice(u);
    }
    Some(v)
}

/// The `hvcC` record for the VPS, SPS and PPS units in `data` (Annex B), length size 4. The
/// profile fields are left zero: the decoders take everything from the parameter sets.
pub fn hvcc_from(data: &[u8]) -> Option<Vec<u8>> {
    let mut sets: [Vec<&[u8]>; 3] = Default::default();
    for (s, e) in nal_ranges(data) {
        let u = &data[s..e];
        let k = match hevc_type(u) {
            32 => 0,
            33 => 1,
            34 => 2,
            _ => continue,
        };
        if !sets[k].contains(&u) {
            sets[k].push(u);
        }
    }
    if sets.iter().any(|s| s.is_empty()) {
        return None;
    }
    let mut v = vec![0u8; 23];
    v[0] = 1;
    v[21] = 0xfc | 3;
    v[22] = 3;
    for (k, ty) in [32u8, 33, 34].into_iter().enumerate() {
        v.push(0x80 | ty);
        v.extend_from_slice(&(sets[k].len() as u16).to_be_bytes());
        for u in &sets[k] {
            v.extend_from_slice(&(u.len() as u16).to_be_bytes());
            v.extend_from_slice(u);
        }
    }
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_three_and_four_byte_start_codes() {
        let d = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 0, 1, 0x65, 9, 9];
        let r = nal_ranges(&d);
        assert_eq!(r, [(4, 7), (10, 12), (17, 20)]);
        let mut out = Vec::new();
        to_length_prefixed(&d, &mut out);
        assert_eq!(out, [0, 0, 0, 3, 0x67, 1, 2, 0, 0, 0, 2, 0x68, 3, 0, 0, 0, 3, 0x65, 9, 9]);
        assert!(is_annexb(&d));
        assert!(!is_annexb(&out));
    }

    #[test]
    fn avcc_has_parameter_sets() {
        let d = [0, 0, 0, 1, 0x67, 0x64, 0, 0x28, 0xac, 0, 0, 0, 1, 0x68, 0xee, 0x3c, 0x80, 0, 0, 0, 1, 0x65, 1];
        let c = avcc_from(&d).unwrap();
        let cfg = crate::nal::avcc(&c).unwrap();
        assert_eq!(cfg.len_size, 4);
        assert_eq!(cfg.units.len(), 2);
        assert_eq!(&c[1..4], [0x64, 0, 0x28]);
    }

    #[test]
    fn hvcc_has_parameter_sets() {
        let d = [0, 0, 0, 1, 0x40, 1, 0xaa, 0, 0, 0, 1, 0x42, 1, 0xbb, 0, 0, 0, 1, 0x44, 1, 0xcc, 0, 0, 1, 0x26, 1, 5];
        let c = hvcc_from(&d).unwrap();
        let cfg = crate::nal::hvcc(&c).unwrap();
        assert_eq!(cfg.len_size, 4);
        assert_eq!(cfg.units.len(), 3);
    }
}
