//! `DXVA_PicParams_AV1` and `DXVA_Tile_AV1` (dxva.h, packed to 1 byte; bit fields spelled out as
//! shifts), filled from a parsed header. The bitstream buffer holds the tile data of the frame's
//! tile groups; one `DXVA_Tile_AV1` per tile says where each tile is in it.

use crate::av1::{Header, Sequence, TileEntry};

/// `DXVA_PicEntry_AV1`.
#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct PicEntry {
    pub width: u32,
    pub height: u32,
    pub wmmat: [i32; 6],
    /// wminvalid:1, wmtype:2.
    pub global_motion_flags: u8,
    pub index: u8,
    pub reserved16: u16,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct Tiles {
    pub cols: u8,
    pub rows: u8,
    pub context_update_id: u16,
    pub widths: [u16; 64],
    pub heights: [u16; 64],
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct LoopFilter {
    pub filter_level: [u8; 2],
    pub filter_level_u: u8,
    pub filter_level_v: u8,
    pub sharpness_level: u8,
    /// mode_ref_delta_enabled:1, mode_ref_delta_update:1, delta_lf_multi:1, delta_lf_present:1.
    pub control_flags: u8,
    pub ref_deltas: [i8; 8],
    pub mode_deltas: [i8; 2],
    pub delta_lf_res: u8,
    pub frame_restoration_type: [u8; 3],
    pub log2_restoration_unit_size: [u16; 3],
    pub reserved16: u16,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct Quantization {
    /// delta_q_present:1, delta_q_res:2.
    pub control_flags: u8,
    pub base_qindex: u8,
    pub y_dc_delta_q: i8,
    pub u_dc_delta_q: i8,
    pub v_dc_delta_q: i8,
    pub u_ac_delta_q: i8,
    pub v_ac_delta_q: i8,
    pub qm_y: u8,
    pub qm_u: u8,
    pub qm_v: u8,
    pub reserved16: u16,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct Cdef {
    /// damping:2, bits:2.
    pub control_flags: u8,
    /// primary:6, secondary:2.
    pub y_strengths: [u8; 8],
    pub uv_strengths: [u8; 8],
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct Segmentation {
    /// enabled:1, update_map:1, update_data:1, temporal_update:1.
    pub control_flags: u8,
    pub reserved24: [u8; 3],
    /// alt_q, alt_lf_y_v, alt_lf_y_h, alt_lf_u, alt_lf_v, ref_frame, skip, globalmv.
    pub feature_mask: [u8; 8],
    pub feature_data: [[i16; 8]; 8],
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct FilmGrain {
    /// apply_grain:1, scaling_shift_minus8:2, chroma_scaling_from_luma:1, ar_coeff_lag:2,
    /// ar_coeff_shift_minus6:2, grain_scale_shift:2, overlap_flag:1, clip_to_restricted_range:1,
    /// matrix_coeff_is_identity:1.
    pub control_flags: u16,
    pub grain_seed: u16,
    pub scaling_points_y: [[u8; 2]; 14],
    pub num_y_points: u8,
    pub scaling_points_cb: [[u8; 2]; 10],
    pub num_cb_points: u8,
    pub scaling_points_cr: [[u8; 2]; 10],
    pub num_cr_points: u8,
    pub ar_coeffs_y: [u8; 24],
    pub ar_coeffs_cb: [u8; 25],
    pub ar_coeffs_cr: [u8; 25],
    pub cb_mult: u8,
    pub cb_luma_mult: u8,
    pub cr_mult: u8,
    pub cr_luma_mult: u8,
    pub reserved8: u8,
    pub cb_offset: i16,
    pub cr_offset: i16,
}

/// `DXVA_PicParams_AV1`.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct PicParams {
    pub width: u32,
    pub height: u32,
    pub max_width: u32,
    pub max_height: u32,
    pub curr_pic_texture_index: u8,
    pub superres_denom: u8,
    pub bitdepth: u8,
    pub seq_profile: u8,
    pub tiles: Tiles,
    pub coding: u32,
    /// frame_type:2, show_frame:1, showable_frame:1, subsampling_x:1, subsampling_y:1,
    /// mono_chrome:1.
    pub format: u8,
    pub primary_ref_frame: u8,
    pub order_hint: u8,
    pub order_hint_bits: u8,
    pub frame_refs: [PicEntry; 7],
    pub ref_frame_map_texture_index: [u8; 8],
    pub loop_filter: LoopFilter,
    pub quantization: Quantization,
    pub cdef: Cdef,
    pub interp_filter: u8,
    pub segmentation: Segmentation,
    pub film_grain: FilmGrain,
    pub reserved32: u32,
    pub status_report_feedback_number: u32,
}

/// `DXVA_Tile_AV1`.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct Tile {
    pub data_offset: u32,
    pub data_size: u32,
    pub row: u16,
    pub column: u16,
    pub reserved16: u16,
    pub anchor_frame: u8,
    pub reserved8: u8,
}

pub fn tile_bytes(v: &[Tile]) -> &[u8] {
    // SAFETY: `Tile` is `repr(C, packed)` plain integers without padding.
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) }
}

pub fn make_tiles(entries: &[TileEntry], group_offset: usize) -> Vec<Tile> {
    entries
        .iter()
        .map(|e| Tile { data_offset: (group_offset + e.offset) as u32, data_size: e.size as u32, row: e.row, column: e.col, reserved16: 0, anchor_frame: 0xff, reserved8: 0 })
        .collect()
}

/// What the picture parameters need besides the header.
pub struct PicInfo<'a> {
    pub seq: &'a Sequence,
    pub header: &'a Header,
    pub surface: usize,
    /// The surface in each reference slot before this frame, and its size.
    pub slots: [Option<usize>; 8],
    pub slot_dims: [(u32, u32); 8],
    pub feedback: u32,
}

pub fn pic_params(p: &PicInfo) -> PicParams {
    let h = p.header;
    let s = p.seq;
    let b = |x: bool| x as u32;
    let mut frame_refs = [PicEntry { index: 0xff, ..Default::default() }; 7];
    if !h.frame_is_intra {
        for i in 0..7 {
            let slot = h.ref_frame_idx[i] as usize;
            let rf = i + 1;
            let mut wmmat = [0i32; 6];
            wmmat.copy_from_slice(&h.gm_params[rf]);
            frame_refs[i] = PicEntry {
                width: p.slot_dims[slot].0,
                height: p.slot_dims[slot].1,
                wmmat,
                global_motion_flags: (h.gm_type[rf] == 0) as u8 | h.gm_type[rf] << 1,
                index: if p.slots[slot].is_some() { slot as u8 } else { 0xff },
                reserved16: 0,
            };
        }
    }
    let mut map = [0xffu8; 8];
    for i in 0..8 {
        if let Some(sf) = p.slots[i] {
            map[i] = sf as u8;
        }
    }
    let mut tiles = Tiles { cols: h.tile_cols as u8, rows: h.tile_rows as u8, context_update_id: h.context_update_tile_id as u16, widths: [0; 64], heights: [0; 64] };
    for (i, w) in h.tile_widths_sb.iter().enumerate() {
        tiles.widths[i] = *w as u16;
    }
    for (i, v) in h.tile_heights_sb.iter().enumerate() {
        tiles.heights[i] = *v as u16;
    }
    let coding = b(s.use_128x128_superblock)
        | b(s.enable_intra_edge_filter) << 1
        | b(s.enable_interintra_compound) << 2
        | b(s.enable_masked_compound) << 3
        | b(h.allow_warped_motion) << 4
        | b(s.enable_dual_filter) << 5
        | b(s.enable_jnt_comp) << 6
        | b(h.allow_screen_content_tools) << 7
        | b(h.force_integer_mv) << 8
        | b(s.enable_cdef) << 9
        | b(s.enable_restoration) << 10
        | b(s.film_grain_params_present) << 11
        | b(h.allow_intrabc) << 12
        | b(h.allow_high_precision_mv) << 13
        | b(h.is_motion_mode_switchable) << 14
        | b(s.enable_filter_intra) << 15
        | b(h.disable_frame_end_update_cdf) << 16
        | b(h.disable_cdf_update) << 17
        | b(h.reference_select) << 18
        | b(h.skip_mode_present) << 19
        | b(h.reduced_tx_set) << 20
        | b(h.use_superres) << 21
        | (h.tx_mode as u32) << 22
        | b(h.use_ref_frame_mvs) << 24
        | b(s.enable_ref_frame_mvs) << 25
        | 1 << 26;
    let mut mask = [0u8; 8];
    for (m, f) in mask.iter_mut().zip(&h.seg.feature_enabled) {
        for (j, e) in f.iter().enumerate() {
            *m |= (*e as u8) << j;
        }
    }
    let mut lr_log2 = [0u16; 3];
    if h.uses_lr {
        lr_log2[0] = 6 + h.lr_unit_shift as u16;
        lr_log2[1] = lr_log2[0] - h.lr_uv_shift as u16;
        lr_log2[2] = lr_log2[1];
    }
    let mut ystr = [0u8; 8];
    let mut uvstr = [0u8; 8];
    for i in 0..8 {
        ystr[i] = h.cdef_y_pri[i] | h.cdef_y_sec[i] << 6;
        uvstr[i] = h.cdef_uv_pri[i] | h.cdef_uv_sec[i] << 6;
    }
    let g = &h.film_grain;
    let mut fg = FilmGrain {
        control_flags: g.apply_grain as u16
            | (g.grain_scaling_minus_8 as u16) << 1
            | (g.chroma_scaling_from_luma as u16) << 3
            | (g.ar_coeff_lag as u16) << 4
            | (g.ar_coeff_shift_minus_6 as u16) << 6
            | (g.grain_scale_shift as u16) << 8
            | (g.overlap_flag as u16) << 10
            | (g.clip_to_restricted_range as u16) << 11
            | ((s.matrix_coefficients == 0) as u16) << 12,
        grain_seed: g.grain_seed,
        scaling_points_y: [[0; 2]; 14],
        num_y_points: g.scaling_points_y.len() as u8,
        scaling_points_cb: [[0; 2]; 10],
        num_cb_points: g.scaling_points_cb.len() as u8,
        scaling_points_cr: [[0; 2]; 10],
        num_cr_points: g.scaling_points_cr.len() as u8,
        ar_coeffs_y: [0; 24],
        ar_coeffs_cb: [0; 25],
        ar_coeffs_cr: [0; 25],
        cb_mult: g.cb_mult,
        cb_luma_mult: g.cb_luma_mult,
        cr_mult: g.cr_mult,
        cr_luma_mult: g.cr_luma_mult,
        reserved8: 0,
        cb_offset: g.cb_offset as i16,
        cr_offset: g.cr_offset as i16,
    };
    for (i, (a, c)) in g.scaling_points_y.iter().take(14).enumerate() {
        fg.scaling_points_y[i] = [*a, *c];
    }
    for (i, (a, c)) in g.scaling_points_cb.iter().take(10).enumerate() {
        fg.scaling_points_cb[i] = [*a, *c];
    }
    for (i, (a, c)) in g.scaling_points_cr.iter().take(10).enumerate() {
        fg.scaling_points_cr[i] = [*a, *c];
    }
    for (i, v) in g.ar_coeffs_y_plus_128.iter().take(24).enumerate() {
        fg.ar_coeffs_y[i] = *v;
    }
    for (i, v) in g.ar_coeffs_cb_plus_128.iter().take(25).enumerate() {
        fg.ar_coeffs_cb[i] = *v;
    }
    for (i, v) in g.ar_coeffs_cr_plus_128.iter().take(25).enumerate() {
        fg.ar_coeffs_cr[i] = *v;
    }
    PicParams {
        width: h.upscaled_width,
        height: h.frame_height,
        max_width: s.max_frame_width,
        max_height: s.max_frame_height,
        curr_pic_texture_index: p.surface as u8,
        superres_denom: h.superres_denom as u8,
        bitdepth: s.bit_depth,
        seq_profile: s.profile,
        tiles,
        coding,
        format: h.frame_type | (h.show_frame as u8) << 2 | (h.showable_frame as u8) << 3 | (s.subsampling_x as u8) << 4 | (s.subsampling_y as u8) << 5 | (s.mono_chrome as u8) << 6,
        primary_ref_frame: h.primary_ref_frame,
        order_hint: h.order_hint as u8,
        order_hint_bits: s.order_hint_bits as u8,
        frame_refs,
        ref_frame_map_texture_index: map,
        loop_filter: LoopFilter {
            filter_level: [h.loop_filter_level[0], h.loop_filter_level[1]],
            filter_level_u: h.loop_filter_level[2],
            filter_level_v: h.loop_filter_level[3],
            sharpness_level: h.sharpness,
            control_flags: h.lf_delta_enabled as u8 | (h.lf_delta_update as u8) << 1 | (h.delta_lf_multi as u8) << 2 | (h.delta_lf_present as u8) << 3,
            ref_deltas: h.ref_deltas,
            mode_deltas: h.mode_deltas,
            delta_lf_res: h.delta_lf_res,
            frame_restoration_type: h.lr_type,
            log2_restoration_unit_size: lr_log2,
            reserved16: 0,
        },
        quantization: Quantization {
            control_flags: h.delta_q_present as u8 | h.delta_q_res << 1,
            base_qindex: h.base_q_idx,
            y_dc_delta_q: h.delta_q_y_dc,
            u_dc_delta_q: h.delta_q_u_dc,
            v_dc_delta_q: h.delta_q_v_dc,
            u_ac_delta_q: h.delta_q_u_ac,
            v_ac_delta_q: h.delta_q_v_ac,
            qm_y: if h.using_qmatrix { h.qm_y } else { 0xff },
            qm_u: if h.using_qmatrix { h.qm_u } else { 0xff },
            qm_v: if h.using_qmatrix { h.qm_v } else { 0xff },
            reserved16: 0,
        },
        cdef: Cdef { control_flags: h.cdef_damping_minus_3 | h.cdef_bits << 2, y_strengths: ystr, uv_strengths: uvstr },
        interp_filter: h.interpolation_filter,
        segmentation: Segmentation {
            control_flags: h.seg.enabled as u8 | (h.seg.update_map as u8) << 1 | (h.seg.update_data as u8) << 2 | (h.seg.temporal_update as u8) << 3,
            reserved24: [0; 3],
            feature_mask: mask,
            feature_data: h.seg.feature_data,
        },
        film_grain: fg,
        reserved32: 0,
        status_report_feedback_number: p.feedback,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_match_dxva_h() {
        assert_eq!(std::mem::size_of::<PicEntry>(), 36);
        assert_eq!(std::mem::size_of::<Tiles>(), 260);
        assert_eq!(std::mem::size_of::<LoopFilter>(), 28);
        assert_eq!(std::mem::size_of::<Quantization>(), 12);
        assert_eq!(std::mem::size_of::<Cdef>(), 17);
        assert_eq!(std::mem::size_of::<Segmentation>(), 140);
        assert_eq!(std::mem::size_of::<FilmGrain>(), 158);
        assert_eq!(std::mem::size_of::<PicParams>(), 912);
        assert_eq!(std::mem::size_of::<Tile>(), 16);
    }
}
