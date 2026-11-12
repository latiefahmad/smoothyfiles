//! `DXVA_PicParams_VP9` (dxva.h, packed to 1 byte), filled from a parsed header. The slice
//! control is `DXVA_Slice_VPx_Short`, the same ten bytes as [`crate::dxva::SliceShort`]: one
//! entry for the whole frame (the video engine reads the compressed header and the tiles
//! itself).

use crate::vp9::Header;

/// `DXVA_segmentation_VP9`.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct Segmentation {
    /// enabled:1, update_map:1, temporal_update:1, abs_delta:1.
    pub flags: u8,
    pub tree_probs: [u8; 7],
    pub pred_probs: [u8; 3],
    pub feature_data: [[i16; 4]; 8],
    /// Per segment: quantizer:1, loop filter:1, reference:1, skip:1.
    pub feature_mask: [u8; 8],
}

/// `DXVA_PicParams_VP9`.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct PicParams {
    /// DXVA_PicEntry_VPx: surface index in 7 bits, 0xff for none.
    pub curr_pic: u8,
    pub profile: u8,
    /// frame_type:1 (1 = not a key frame), show_frame:1, error_resilient_mode:1,
    /// subsampling_x:1, subsampling_y:1, extra_plane:1, refresh_frame_context:1,
    /// frame_parallel_decoding_mode:1, intra_only:1, frame_context_idx:2,
    /// reset_frame_context:2, allow_high_precision_mv:1.
    pub format_and_picture_flags: u16,
    pub width: u32,
    pub height: u32,
    pub bit_depth_minus8_luma: u8,
    pub bit_depth_minus8_chroma: u8,
    pub interp_filter: u8,
    pub reserved8: u8,
    pub ref_frame_map: [u8; 8],
    pub ref_frame_coded_width: [u32; 8],
    pub ref_frame_coded_height: [u32; 8],
    pub frame_refs: [u8; 3],
    pub ref_frame_sign_bias: [i8; 4],
    pub filter_level: i8,
    pub sharpness_level: u8,
    /// mode_ref_delta_enabled:1, mode_ref_delta_update:1, use_prev_in_find_mv_refs:1.
    pub control_flags: u8,
    pub ref_deltas: [i8; 4],
    pub mode_deltas: [i8; 2],
    pub base_qindex: i16,
    pub y_dc_delta_q: i8,
    pub uv_dc_delta_q: i8,
    pub uv_ac_delta_q: i8,
    pub segments: Segmentation,
    pub log2_tile_cols: u8,
    pub log2_tile_rows: u8,
    pub uncompressed_header_size: u16,
    pub first_partition_size: u16,
    pub reserved16: u16,
    pub reserved32: u32,
    pub status_report_feedback_number: u32,
}

/// What the picture parameters need besides the header.
pub struct PicInfo<'a> {
    pub header: &'a Header,
    pub surface: usize,
    /// The surface in each reference slot before this frame, and its size.
    pub slots: [Option<usize>; 8],
    pub slot_dims: [(u32, u32); 8],
    pub use_prev_mvs: bool,
    pub feedback: u32,
}

pub fn pic_params(p: &PicInfo) -> PicParams {
    let h = p.header;
    let b = |x: bool| x as u16;
    let mut ref_frame_map = [0xffu8; 8];
    let mut rw = [0u32; 8];
    let mut rh = [0u32; 8];
    for i in 0..8 {
        if let Some(s) = p.slots[i] {
            ref_frame_map[i] = s as u8 & 0x7f;
            rw[i] = p.slot_dims[i].0;
            rh[i] = p.slot_dims[i].1;
        }
    }
    let mut frame_refs = [0xffu8; 3];
    if !h.is_intra() {
        for i in 0..3 {
            frame_refs[i] = ref_frame_map[h.ref_frame_idx[i] as usize];
        }
    }
    let seg = &h.seg;
    let mut mask = [0u8; 8];
    for (m, f) in mask.iter_mut().zip(&seg.feature_enabled) {
        *m = f[0] as u8 | (f[1] as u8) << 1 | (f[2] as u8) << 2 | (f[3] as u8) << 3;
    }
    PicParams {
        curr_pic: p.surface as u8 & 0x7f,
        profile: h.profile,
        format_and_picture_flags: b(!h.key_frame)
            | b(h.show_frame) << 1
            | b(h.error_resilient) << 2
            | b(h.subsampling_x) << 3
            | b(h.subsampling_y) << 4
            | b(h.refresh_frame_context) << 6
            | b(h.frame_parallel_decoding) << 7
            | b(h.intra_only) << 8
            | (h.frame_context_idx as u16) << 9
            | (h.reset_frame_context as u16) << 11
            | b(h.allow_high_precision_mv) << 13,
        width: h.width,
        height: h.height,
        bit_depth_minus8_luma: h.bit_depth - 8,
        bit_depth_minus8_chroma: h.bit_depth - 8,
        interp_filter: h.interp_filter,
        reserved8: 0,
        ref_frame_map,
        ref_frame_coded_width: rw,
        ref_frame_coded_height: rh,
        frame_refs,
        ref_frame_sign_bias: [h.sign_bias[0] as i8, h.sign_bias[1] as i8, h.sign_bias[2] as i8, h.sign_bias[3] as i8],
        filter_level: h.filter_level as i8,
        sharpness_level: h.sharpness,
        control_flags: h.lf_delta_enabled as u8 | (h.lf_delta_update as u8) << 1 | (p.use_prev_mvs as u8) << 2,
        ref_deltas: h.ref_deltas,
        mode_deltas: h.mode_deltas,
        base_qindex: h.base_qindex as i16,
        y_dc_delta_q: h.y_dc_delta_q,
        uv_dc_delta_q: h.uv_dc_delta_q,
        uv_ac_delta_q: h.uv_ac_delta_q,
        segments: Segmentation {
            flags: seg.enabled as u8 | (seg.update_map as u8) << 1 | (seg.temporal_update as u8) << 2 | (seg.abs_delta as u8) << 3,
            tree_probs: seg.tree_probs,
            pred_probs: seg.pred_probs,
            feature_data: seg.feature_data,
            feature_mask: mask,
        },
        log2_tile_cols: h.log2_tile_cols,
        log2_tile_rows: h.log2_tile_rows,
        uncompressed_header_size: h.uncompressed_size as u16,
        first_partition_size: h.compressed_header_size,
        reserved16: 0,
        reserved32: 0,
        status_report_feedback_number: p.feedback,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_match_dxva_h() {
        // dxva.h packs to one byte: 1+1+2+4+4+4+8+32+32+3+4+1+1+1+4+2+2+3+83+2+2+2+2+4+4
        assert_eq!(std::mem::size_of::<Segmentation>(), 1 + 7 + 3 + 64 + 8);
        assert_eq!(std::mem::size_of::<PicParams>(), 208);
    }
}
