//! The DXVA structures for H.264 (`DXVA_PicParams_H264`, `DXVA_Qmatrix_H264`; the slice control
//! is the short one from [`crate::dxva`]), laid out as dxva.h packs them (1-byte packing, bit
//! fields spelled out as shifts), and filled from the parsed parameter sets and slice header.

use crate::dpb::RefMark;
use crate::h264::{Matrices, Pps, SliceHeader, Sps};
use crate::h264_dpb::{Pic, Started};

/// `DXVA_PicParams_H264`.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct PicParams {
    pub frame_width_in_mbs_minus1: u16,
    pub frame_height_in_mbs_minus1: u16,
    /// DXVA_PicEntry_H264: surface index in 7 bits, bottom field flag on top.
    pub curr_pic: u8,
    pub num_ref_frames: u8,
    /// field_pic:1, MbaffFrame:1, residual_colour_transform:1, sp_for_switch:1,
    /// chroma_format_idc:2, RefPic:1, constrained_intra_pred:1, weighted_pred:1,
    /// weighted_bipred_idc:2, MbsConsecutive:1, frame_mbs_only:1, transform_8x8_mode:1,
    /// MinLumaBipredSize8x8:1, IntraPic:1.
    pub bit_fields: u16,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub reserved16_bits: u16,
    pub status_report_feedback_number: u32,
    /// Surface index (7 bits) and long-term flag (top bit); 0xff when unused.
    pub ref_frame_list: [u8; 16],
    pub curr_field_order_cnt: [i32; 2],
    pub field_order_cnt_list: [[i32; 2]; 16],
    pub pic_init_qs_minus26: i8,
    pub chroma_qp_index_offset: i8,
    pub second_chroma_qp_index_offset: i8,
    pub continuation_flag: u8,
    pub pic_init_qp_minus26: i8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub reserved8_bits_a: u8,
    pub frame_num_list: [u16; 16],
    pub used_for_reference_flags: u32,
    pub non_existing_frame_flags: u16,
    pub frame_num: u16,
    pub log2_max_frame_num_minus4: u8,
    pub pic_order_cnt_type: u8,
    pub log2_max_pic_order_cnt_lsb_minus4: u8,
    pub delta_pic_order_always_zero_flag: u8,
    pub direct_8x8_inference_flag: u8,
    pub entropy_coding_mode_flag: u8,
    pub pic_order_present_flag: u8,
    pub num_slice_groups_minus1: u8,
    pub slice_group_map_type: u8,
    pub deblocking_filter_control_present_flag: u8,
    pub redundant_pic_cnt_present_flag: u8,
    pub reserved8_bits_b: u8,
    pub slice_group_change_rate_minus1: u16,
    pub slice_group_map: [u8; 810],
}

/// `DXVA_Qmatrix_H264`: scaling factors in coded (zig-zag) order.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct QMatrix {
    pub lists4x4: [[u8; 16]; 6],
    pub lists8x8: [[u8; 64]; 2],
}

fn entry(surface: usize, long_term: bool) -> u8 {
    (surface as u8 & 0x7f) | ((long_term as u8) << 7)
}

/// Everything about the picture that goes into the picture parameters.
pub struct PicInfo<'a> {
    pub sps: &'a Sps,
    pub pps: &'a Pps,
    pub sh: &'a SliceHeader,
    pub started: &'a Started,
    /// Every slice of the picture is an I slice.
    pub intra: bool,
    pub surface: usize,
    /// The frames kept as reference, in [`crate::h264_dpb::Dpb::ref_pics`] order.
    pub refs: &'a [&'a Pic],
    pub feedback: u32,
}

pub fn pic_params(p: &PicInfo) -> PicParams {
    let (sps, pps) = (p.sps, p.pps);
    let b = |v: bool| v as u16;
    let mut pp: PicParams = unsafe { std::mem::zeroed() };
    pp.frame_width_in_mbs_minus1 = (sps.width_mbs - 1) as u16;
    pp.frame_height_in_mbs_minus1 = (sps.height_mbs - 1) as u16;
    // a field picture names its parity in the top bit (AssociatedFlag)
    pp.curr_pic = entry(p.surface, p.sh.field_pic && p.sh.bottom_field);
    pp.num_ref_frames = sps.max_num_ref_frames;
    pp.bit_fields = b(p.sh.field_pic)
        | b(sps.mbaff && !p.sh.field_pic) << 1
        | b(sps.separate_colour_plane) << 2
        | (sps.chroma_format_idc as u16) << 4
        | b(p.sh.nal_ref_idc != 0) << 6
        | b(pps.constrained_intra_pred) << 7
        | b(pps.weighted_pred) << 8
        | (pps.weighted_bipred_idc as u16) << 9
        | 1 << 11
        | b(sps.frame_mbs_only) << 12
        | b(pps.transform_8x8_mode) << 13
        | b(sps.level_idc >= 31) << 14
        | b(p.intra) << 15;
    pp.bit_depth_luma_minus8 = sps.bit_depth_luma - 8;
    pp.bit_depth_chroma_minus8 = sps.bit_depth_chroma - 8;
    pp.reserved16_bits = 3;
    pp.status_report_feedback_number = p.feedback;
    pp.ref_frame_list = [0xff; 16];
    pp.curr_field_order_cnt = match (p.sh.field_pic, p.sh.bottom_field) {
        (false, _) => [p.started.top_poc, p.started.bottom_poc],
        (true, false) => [p.started.top_poc, 0],
        (true, true) => [0, p.started.bottom_poc],
    };
    let mut used = 0u32;
    for (i, r) in p.refs.iter().take(16).enumerate() {
        let long = r.mark() == RefMark::Long;
        pp.ref_frame_list[i] = entry(r.surface, long);
        // order counts and reference flags per field: a field that is not a reference counts as absent
        let (top, bottom) = (r.marks[0] != RefMark::Unused, r.marks[1] != RefMark::Unused);
        pp.field_order_cnt_list[i] = [if top { r.top_poc } else { 0 }, if bottom { r.bottom_poc } else { 0 }];
        pp.frame_num_list[i] = if long { r.long_idx as u16 } else { r.frame_num as u16 };
        used |= (top as u32 | (bottom as u32) << 1) << (2 * i);
    }
    pp.used_for_reference_flags = used;
    pp.pic_init_qs_minus26 = pps.pic_init_qs_minus26;
    pp.chroma_qp_index_offset = pps.chroma_qp_index_offset;
    pp.second_chroma_qp_index_offset = pps.second_chroma_qp_index_offset;
    pp.continuation_flag = 1;
    pp.pic_init_qp_minus26 = pps.pic_init_qp_minus26;
    pp.num_ref_idx_l0_active_minus1 = pps.num_ref_idx_l0_default - 1;
    pp.num_ref_idx_l1_active_minus1 = pps.num_ref_idx_l1_default - 1;
    pp.frame_num = p.sh.frame_num as u16;
    pp.log2_max_frame_num_minus4 = sps.log2_max_frame_num - 4;
    pp.pic_order_cnt_type = sps.poc_type;
    pp.log2_max_pic_order_cnt_lsb_minus4 = sps.log2_max_poc_lsb.saturating_sub(4);
    pp.delta_pic_order_always_zero_flag = sps.delta_pic_order_always_zero as u8;
    pp.direct_8x8_inference_flag = sps.direct_8x8_inference as u8;
    pp.entropy_coding_mode_flag = pps.entropy_coding_mode as u8;
    pp.pic_order_present_flag = pps.bottom_field_pic_order_in_frame_present as u8;
    pp.deblocking_filter_control_present_flag = pps.deblocking_filter_control_present as u8;
    pp.redundant_pic_cnt_present_flag = pps.redundant_pic_cnt_present as u8;
    pp
}

pub fn qmatrix(m: &Matrices) -> QMatrix {
    QMatrix { lists4x4: m.l4, lists8x8: m.l8 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_match_dxva_h() {
        assert_eq!(std::mem::size_of::<QMatrix>(), 224);
        assert_eq!(std::mem::size_of::<PicParams>(), 1040);
    }
}
