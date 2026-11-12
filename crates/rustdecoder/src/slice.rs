//! HEVC slice segment header (ITU-T H.265, 7.3.6.1), as far as picture management needs it.

use crate::bits::BitReader;
use crate::hevc::{NalHeader, Pps, Sps, StRps, nal_type};
use crate::{Error, Result, r};

/// One long-term reference named in a slice header, resolved against the SPS's candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LongTermRef {
    pub poc_lsb: u32,
    pub used_by_curr: bool,
    pub msb_present: bool,
    /// DeltaPocMsbCycleLt (7-52): already summed over the entries before it.
    pub delta_poc_msb_cycle: u32,
}

/// The start of a slice segment header. What follows the long-term pictures is read by the
/// GPU itself from the bitstream, so it is not parsed here. Only the first slice segment of a
/// picture is read in full.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SliceHeader {
    pub first_slice_segment_in_pic: bool,
    pub no_output_of_prior_pics: bool,
    pub pps_id: u8,
    pub slice_type: u32,
    pub pic_output: bool,
    pub poc_lsb: u32,
    /// The short-term reference picture set in use (empty for IDR pictures).
    pub st_rps: StRps,
    /// Index into the SPS's sets, or `None` when the set was sent in this header.
    pub st_rps_sps_idx: Option<usize>,
    /// Bits the set takes in this header (0 when taken from the SPS).
    pub st_rps_bits: u32,
    /// NumDeltaPocs of the set the slice's set was predicted from (0 when not predicted).
    pub st_rps_ref_num_delta_pocs: usize,
    pub long_term: Vec<LongTermRef>,
}

fn ceil_log2(n: usize) -> u32 {
    if n <= 1 { 0 } else { usize::BITS - (n - 1).leading_zeros() }
}

/// Reads the slice segment header after the NAL header, with the parameter sets it refers to
/// found through `pps_of` and `sps_of`. A slice segment that does not start a picture gives a
/// header with `first_slice_segment_in_pic` false and nothing else filled in.
pub fn slice_header<'a>(
    b: &mut BitReader,
    nal: &NalHeader,
    pps_of: impl Fn(u8) -> Option<&'a Pps>,
    sps_of: impl Fn(u8) -> Option<&'a Sps>,
) -> Result<SliceHeader> {
    let first = r!(b, flag, "first_slice_segment_in_pic_flag");
    let irap = (nal_type::BLA_W_LP..=nal_type::RSV_IRAP_23).contains(&nal.nal_unit_type);
    let no_output_of_prior_pics = irap && r!(b, flag, "no_output_of_prior_pics_flag");
    let pps_id = r!(b, ue, "slice_pic_parameter_set_id");
    let mut h = SliceHeader {
        first_slice_segment_in_pic: first,
        no_output_of_prior_pics,
        pps_id: pps_id.min(255) as u8,
        slice_type: 0,
        pic_output: true,
        poc_lsb: 0,
        st_rps: StRps::default(),
        st_rps_sps_idx: None,
        st_rps_bits: 0,
        st_rps_ref_num_delta_pocs: 0,
        long_term: Vec::new(),
    };
    if !first {
        return Ok(h);
    }
    let pps = pps_of(h.pps_id).ok_or(Error::Invalid("slice refers to a missing PPS"))?;
    let sps = sps_of(pps.sps_id).ok_or(Error::Invalid("slice refers to a missing SPS"))?;
    for i in 0..pps.num_extra_slice_header_bits {
        r!(b, flag, "slice_reserved_flag[{}]", i);
    }
    h.slice_type = r!(b, ue, "slice_type");
    if pps.output_flag_present {
        h.pic_output = r!(b, flag, "pic_output_flag");
    }
    if sps.separate_colour_plane {
        r!(b, u 2, "colour_plane_id");
    }
    let idr = matches!(nal.nal_unit_type, nal_type::IDR_W_RADL | nal_type::IDR_N_LP);
    if idr {
        return Ok(h);
    }
    h.poc_lsb = r!(b, u sps.log2_max_poc_lsb as u32, "slice_pic_order_cnt_lsb");
    let n = sps.st_rps.len();
    if !r!(b, flag, "short_term_ref_pic_set_sps_flag") {
        let start = b.position();
        let (rps, rf) = StRps::read_with_ref(b, n, n, &sps.st_rps)?;
        h.st_rps_bits = (b.position() - start) as u32;
        h.st_rps_ref_num_delta_pocs = rf;
        h.st_rps = rps;
    } else {
        if n == 0 {
            return Err(Error::Invalid("short_term_ref_pic_set_sps_flag without sets"));
        }
        let idx = if n > 1 { r!(b, u ceil_log2(n), "short_term_ref_pic_set_idx") as usize } else { 0 };
        h.st_rps = sps.st_rps.get(idx).ok_or(Error::Invalid("short_term_ref_pic_set_idx"))?.clone();
        h.st_rps_sps_idx = Some(idx);
    }
    if sps.long_term_refs_present {
        let cand = sps.lt_ref_pics.len();
        let num_sps = if cand > 0 { r!(b, ue, "num_long_term_sps") as usize } else { 0 };
        let num_pics = r!(b, ue, "num_long_term_pics") as usize;
        if num_sps > cand || num_sps + num_pics > 32 {
            return Err(Error::Invalid("long-term picture count"));
        }
        let mut prev_cycle = 0u32;
        for i in 0..num_sps + num_pics {
            let (poc_lsb, used) = if i < num_sps {
                let idx = if cand > 1 { r!(b, u ceil_log2(cand), "lt_idx_sps[{}]", i) as usize } else { 0 };
                *sps.lt_ref_pics.get(idx).ok_or(Error::Invalid("lt_idx_sps"))?
            } else {
                let lsb = r!(b, u sps.log2_max_poc_lsb as u32, "poc_lsb_lt[{}]", i);
                (lsb, r!(b, flag, "used_by_curr_pic_lt_flag[{}]", i))
            };
            let msb_present = r!(b, flag, "delta_poc_msb_present_flag[{}]", i);
            let mut cycle = if msb_present { r!(b, ue, "delta_poc_msb_cycle_lt[{}]", i) } else { 0 };
            if i != 0 && i != num_sps {
                cycle = cycle.wrapping_add(prev_cycle);
            }
            prev_cycle = cycle;
            h.long_term.push(LongTermRef { poc_lsb, used_by_curr: used, msb_present, delta_poc_msb_cycle: cycle });
        }
    }
    Ok(h)
}
