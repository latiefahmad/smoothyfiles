//! The DXVA structures for HEVC (`DXVA_PicParams_HEVC`, `DXVA_Qmatrix_HEVC`,
//! `DXVA_Slice_HEVC_Short`), laid out as dxva.h packs them (1-byte packing, bit fields spelled
//! out as shifts), and filled from the parsed parameter sets and slice header.

use crate::dpb::{Pic, PicKind, RefMark, RefSets};
use crate::hevc::{Pps, ScalingList, Sps};
use crate::slice::SliceHeader;

/// `DXVA_PicParams_HEVC`.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct PicParams {
    pub pic_width_in_min_cbs_y: u16,
    pub pic_height_in_min_cbs_y: u16,
    /// chroma_format_idc:2, separate_colour_plane:1, bit_depth_luma_minus8:3,
    /// bit_depth_chroma_minus8:3, log2_max_poc_lsb_minus4:4, NoPicReordering:1, NoBiPred:1.
    pub format_and_sequence_flags: u16,
    /// DXVA_PicEntry_HEVC: surface index in 7 bits, "associated" (long-term) flag on top.
    pub curr_pic: u8,
    pub sps_max_dec_pic_buffering_minus1: u8,
    pub log2_min_luma_coding_block_size_minus3: u8,
    pub log2_diff_max_min_luma_coding_block_size: u8,
    pub log2_min_transform_block_size_minus2: u8,
    pub log2_diff_max_min_transform_block_size: u8,
    pub max_transform_hierarchy_depth_inter: u8,
    pub max_transform_hierarchy_depth_intra: u8,
    pub num_short_term_ref_pic_sets: u8,
    pub num_long_term_ref_pics_sps: u8,
    pub num_ref_idx_l0_default_active_minus1: u8,
    pub num_ref_idx_l1_default_active_minus1: u8,
    pub init_qp_minus26: i8,
    pub num_delta_pocs_of_ref_rps_idx: u8,
    pub num_bits_for_short_term_rps_in_slice: u16,
    pub reserved_bits2: u16,
    pub coding_param_tool_flags: u32,
    pub coding_setting_picture_property_flags: u32,
    pub pps_cb_qp_offset: i8,
    pub pps_cr_qp_offset: i8,
    pub num_tile_columns_minus1: u8,
    pub num_tile_rows_minus1: u8,
    pub column_width_minus1: [u16; 19],
    pub row_height_minus1: [u16; 21],
    pub diff_cu_qp_delta_depth: u8,
    pub pps_beta_offset_div2: i8,
    pub pps_tc_offset_div2: i8,
    pub log2_parallel_merge_level_minus2: u8,
    pub curr_pic_order_cnt_val: i32,
    pub ref_pic_list: [u8; 15],
    pub reserved_bits5: u8,
    pub pic_order_cnt_val_list: [i32; 15],
    pub ref_pic_set_st_curr_before: [u8; 8],
    pub ref_pic_set_st_curr_after: [u8; 8],
    pub ref_pic_set_lt_curr: [u8; 8],
    pub reserved_bits6: u16,
    pub reserved_bits7: u16,
    pub status_report_feedback_number: u32,
}

/// `DXVA_Qmatrix_HEVC`: scaling factors in coded (diagonal) order.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct QMatrix {
    pub lists0: [[u8; 16]; 6],
    pub lists1: [[u8; 64]; 6],
    pub lists2: [[u8; 64]; 6],
    pub lists3: [[u8; 64]; 2],
    pub dc_coef_size_id2: [u8; 6],
    pub dc_coef_size_id3: [u8; 2],
}

/// `DXVA_Slice_HEVC_Short`: where a slice NAL unit (with its start code) sits in the bitstream
/// buffer.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct SliceShort {
    pub bs_nal_unit_data_location: u32,
    pub slice_bytes_in_buffer: u32,
    pub bad_slice_chopping: u16,
}

/// The bytes of a plain-data structure.
pub fn bytes_of<T: Copy>(v: &T) -> &[u8] {
    // SAFETY: the structures here are `repr(C, packed)` plain integers without padding.
    unsafe { std::slice::from_raw_parts(v as *const T as *const u8, std::mem::size_of::<T>()) }
}

pub fn slice_bytes(v: &[SliceShort]) -> &[u8] {
    // SAFETY: as above, for a slice of them.
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) }
}

fn entry(surface: usize, long_term: bool) -> u8 {
    (surface as u8 & 0x7f) | ((long_term as u8) << 7)
}

/// Everything about the picture that goes into the picture parameters.
pub struct PicInfo<'a> {
    pub sps: &'a Sps,
    pub pps: &'a Pps,
    pub kind: PicKind,
    pub sh: &'a SliceHeader,
    pub poc: i32,
    /// Surface the picture is decoded into.
    pub surface: usize,
    /// The pictures kept as reference, in [`crate::dpb::Dpb::ref_pics`] order.
    pub refs: &'a [&'a Pic],
    pub sets: &'a RefSets,
    pub feedback: u32,
}

pub fn pic_params(p: &PicInfo) -> PicParams {
    let (sps, pps, sh) = (p.sps, p.pps, p.sh);
    let b = |v: bool| v as u32;
    let mut pp: PicParams = unsafe { std::mem::zeroed() };
    pp.pic_width_in_min_cbs_y = (sps.width >> sps.log2_min_cb_size) as u16;
    pp.pic_height_in_min_cbs_y = (sps.height >> sps.log2_min_cb_size) as u16;
    pp.format_and_sequence_flags = (sps.chroma_format_idc as u16)
        | (sps.separate_colour_plane as u16) << 2
        | ((sps.bit_depth_luma - 8) as u16) << 3
        | ((sps.bit_depth_chroma - 8) as u16) << 6
        | ((sps.log2_max_poc_lsb - 4) as u16) << 9
        | ((sps.max_num_reorder_pics == 0) as u16) << 13;
    pp.curr_pic = entry(p.surface, false);
    pp.sps_max_dec_pic_buffering_minus1 = sps.max_dec_pic_buffering_minus1;
    pp.log2_min_luma_coding_block_size_minus3 = sps.log2_min_cb_size - 3;
    pp.log2_diff_max_min_luma_coding_block_size = sps.log2_diff_max_min_cb_size;
    pp.log2_min_transform_block_size_minus2 = sps.log2_min_tb_size - 2;
    pp.log2_diff_max_min_transform_block_size = sps.log2_diff_max_min_tb_size;
    pp.max_transform_hierarchy_depth_inter = sps.max_transform_hierarchy_depth_inter;
    pp.max_transform_hierarchy_depth_intra = sps.max_transform_hierarchy_depth_intra;
    pp.num_short_term_ref_pic_sets = sps.st_rps.len() as u8;
    pp.num_long_term_ref_pics_sps = sps.lt_ref_pics.len() as u8;
    pp.num_ref_idx_l0_default_active_minus1 = pps.num_ref_idx_l0_default_active_minus1;
    pp.num_ref_idx_l1_default_active_minus1 = pps.num_ref_idx_l1_default_active_minus1;
    pp.init_qp_minus26 = pps.init_qp_minus26;
    pp.num_delta_pocs_of_ref_rps_idx = sh.st_rps_ref_num_delta_pocs as u8;
    pp.num_bits_for_short_term_rps_in_slice = sh.st_rps_bits as u16;

    let pcm = sps.pcm.as_ref();
    pp.coding_param_tool_flags = b(sps.scaling_list_enabled)
        | b(sps.amp_enabled) << 1
        | b(sps.sao_enabled) << 2
        | b(pcm.is_some()) << 3
        | pcm.map_or(0, |c| (c.bit_depth_luma as u32 - 1) << 4 | (c.bit_depth_chroma as u32 - 1) << 8 | (c.log2_min_size as u32 - 3) << 12 | (c.log2_diff_max_min_size as u32) << 14 | b(c.loop_filter_disabled) << 16)
        | b(sps.long_term_refs_present) << 17
        | b(sps.temporal_mvp_enabled) << 18
        | b(sps.strong_intra_smoothing) << 19
        | b(pps.dependent_slice_segments_enabled) << 20
        | b(pps.output_flag_present) << 21
        | (pps.num_extra_slice_header_bits as u32) << 22
        | b(pps.sign_data_hiding_enabled) << 25
        | b(pps.cabac_init_present) << 26;

    let tiles = pps.tiles.as_ref();
    pp.coding_setting_picture_property_flags = b(pps.constrained_intra_pred)
        | b(pps.transform_skip_enabled) << 1
        | b(pps.cu_qp_delta_enabled) << 2
        | b(pps.slice_chroma_qp_offsets_present) << 3
        | b(pps.weighted_pred) << 4
        | b(pps.weighted_bipred) << 5
        | b(pps.transquant_bypass_enabled) << 6
        | b(pps.tiles_enabled) << 7
        | b(pps.entropy_coding_sync_enabled) << 8
        | b(tiles.is_some_and(|t| t.uniform_spacing)) << 9
        | b(tiles.is_some_and(|t| t.loop_filter_across_tiles)) << 10
        | b(pps.loop_filter_across_slices) << 11
        | b(pps.deblocking_filter_override_enabled) << 12
        | b(pps.deblocking_filter_disabled) << 13
        | b(pps.lists_modification_present) << 14
        | b(pps.slice_segment_header_extension_present) << 15
        | b(p.kind.is_irap()) << 16
        | b(p.kind.is_idr()) << 17
        | b(p.kind.is_irap()) << 18;

    pp.pps_cb_qp_offset = pps.cb_qp_offset;
    pp.pps_cr_qp_offset = pps.cr_qp_offset;
    if let Some(t) = tiles {
        pp.num_tile_columns_minus1 = (t.columns - 1) as u8;
        pp.num_tile_rows_minus1 = (t.rows - 1) as u8;
        let ctb = sps.log2_ctb_size() as u32;
        let wc = (sps.width + (1 << ctb) - 1) >> ctb;
        let hc = (sps.height + (1 << ctb) - 1) >> ctb;
        let (cols, rows) = (t.columns as u32, t.rows as u32);
        let (mut used_w, mut used_h) = (0u32, 0u32);
        for i in 0..(cols - 1) as usize {
            let w = if t.uniform_spacing { (i as u32 + 1) * wc / cols - i as u32 * wc / cols } else { t.column_widths[i] as u32 };
            used_w += w;
            if i < 19 {
                pp.column_width_minus1[i] = (w - 1) as u16;
            }
        }
        let _ = used_w;
        for i in 0..(rows - 1) as usize {
            let h = if t.uniform_spacing { (i as u32 + 1) * hc / rows - i as u32 * hc / rows } else { t.row_heights[i] as u32 };
            used_h += h;
            if i < 21 {
                pp.row_height_minus1[i] = (h - 1) as u16;
            }
        }
        let _ = used_h;
    }
    pp.diff_cu_qp_delta_depth = pps.diff_cu_qp_delta_depth;
    pp.pps_beta_offset_div2 = pps.beta_offset_div2;
    pp.pps_tc_offset_div2 = pps.tc_offset_div2;
    pp.log2_parallel_merge_level_minus2 = pps.log2_parallel_merge_level - 2;
    pp.curr_pic_order_cnt_val = p.poc;

    pp.ref_pic_list = [0xff; 15];
    for (i, r) in p.refs.iter().take(15).enumerate() {
        pp.ref_pic_list[i] = entry(r.surface, r.mark == RefMark::Long);
        pp.pic_order_cnt_val_list[i] = r.poc;
    }
    let index_of = |surface: usize| p.refs.iter().take(15).position(|r| r.surface == surface).map_or(0xff, |i| i as u8);
    let fill = |dst: &mut [u8; 8], src: &[usize]| {
        *dst = [0xff; 8];
        for (d, &s) in dst.iter_mut().zip(src) {
            *d = index_of(s);
        }
    };
    fill(&mut pp.ref_pic_set_st_curr_before, &p.sets.st_curr_before);
    fill(&mut pp.ref_pic_set_st_curr_after, &p.sets.st_curr_after);
    fill(&mut pp.ref_pic_set_lt_curr, &p.sets.lt_curr);
    pp.status_report_feedback_number = p.feedback;
    pp
}

/// The scaling lists in effect: the PPS's, else the SPS's, else the defaults; `None` when
/// scaling lists are off.
pub fn qmatrix(sps: &Sps, pps: &Pps) -> Option<QMatrix> {
    if !sps.scaling_list_enabled {
        return None;
    }
    let default;
    let sl: &ScalingList = match (&pps.scaling_list, &sps.scaling_list) {
        (Some(l), _) | (None, Some(l)) => l,
        (None, None) => {
            default = ScalingList::default();
            &default
        }
    };
    Some(QMatrix {
        lists0: sl.s4,
        lists1: sl.s8,
        lists2: sl.s16,
        lists3: sl.s32,
        dc_coef_size_id2: sl.dc16,
        dc_coef_size_id3: sl.dc32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_match_dxva_h() {
        assert_eq!(std::mem::size_of::<QMatrix>(), 1000);
        assert_eq!(std::mem::size_of::<SliceShort>(), 10);
        assert_eq!(std::mem::size_of::<PicParams>(), 232);
    }
}
