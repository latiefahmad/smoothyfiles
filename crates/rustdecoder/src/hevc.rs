//! HEVC (H.265) parameter sets: what the GPU decoder needs to be set up and fed (ITU-T H.265,
//! 7.3.2). Element names follow the standard, so a traced read lines up with ffmpeg's
//! `trace_headers`.

use crate::bits::BitReader;
use crate::{Error, Result, r};

pub mod nal_type {
    pub const BLA_W_LP: u8 = 16;
    pub const IDR_W_RADL: u8 = 19;
    pub const IDR_N_LP: u8 = 20;
    pub const CRA_NUT: u8 = 21;
    pub const RADL_N: u8 = 6;
    pub const RADL_R: u8 = 7;
    pub const RASL_N: u8 = 8;
    pub const RASL_R: u8 = 9;
    pub const EOS: u8 = 36;
    pub const EOB: u8 = 37;
    pub const RSV_IRAP_23: u8 = 23;
    pub const VPS: u8 = 32;
    pub const SPS: u8 = 33;
    pub const PPS: u8 = 34;
    pub const AUD: u8 = 35;
    pub const PREFIX_SEI: u8 = 39;
    pub const SUFFIX_SEI: u8 = 40;
}

/// The two-byte NAL header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NalHeader {
    pub nal_unit_type: u8,
    pub nuh_layer_id: u8,
    pub temporal_id: u8,
}

pub fn nal_header(b: &mut BitReader) -> Result<NalHeader> {
    let forbidden = r!(b, u 1, "forbidden_zero_bit");
    if forbidden != 0 {
        return Err(Error::Invalid("forbidden_zero_bit set"));
    }
    let nal_unit_type = r!(b, u 6, "nal_unit_type") as u8;
    let nuh_layer_id = r!(b, u 6, "nuh_layer_id") as u8;
    let t = r!(b, u 3, "nuh_temporal_id_plus1") as u8;
    if t == 0 {
        return Err(Error::Invalid("nuh_temporal_id_plus1 is 0"));
    }
    Ok(NalHeader { nal_unit_type, nuh_layer_id, temporal_id: t - 1 })
}

/// The NAL unit type, from the first byte.
pub fn nal_unit_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|b| (b >> 1) & 0x3f)
}

// ------------------------------------------------------------------ scaling lists

/// Scaling factors in coded (up-right diagonal) order, as the DXVA quantisation matrix wants them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScalingList {
    /// 4×4, per matrixId 0..6.
    pub s4: [[u8; 16]; 6],
    /// 8×8.
    pub s8: [[u8; 64]; 6],
    /// 16×16 (8×8 coefficients, upsampled by the decoder).
    pub s16: [[u8; 64]; 6],
    /// 32×32: matrixId 0 and 3 (intra, inter luma).
    pub s32: [[u8; 64]; 2],
    pub dc16: [u8; 6],
    pub dc32: [u8; 2],
}

/// Table 7-6: the default 8×8 intra lists (matrixId 0..2), coded order.
const DEFAULT_INTRA: [u8; 64] = [
    16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 17, 16, 17, 16, 17, 18, 17, 18, 18, 17, 18, 21, 19, 20, 21, 20, 19, 21, 24, 22, 22, 24, 24, 22, 22, 24, 25, 25,
    27, 30, 27, 25, 25, 29, 31, 35, 35, 31, 29, 36, 41, 44, 41, 36, 47, 54, 54, 47, 65, 70, 65, 88, 88, 115,
];
/// Table 7-6: the default 8×8 inter lists (matrixId 3..5), coded order.
const DEFAULT_INTER: [u8; 64] = [
    16, 16, 16, 16, 16, 16, 16, 16, 16, 16, 17, 17, 17, 17, 17, 18, 18, 18, 18, 18, 18, 20, 20, 20, 20, 20, 20, 20, 24, 24, 24, 24, 24, 24, 24, 24, 25, 25,
    25, 25, 25, 25, 25, 28, 28, 28, 28, 28, 28, 33, 33, 33, 33, 33, 41, 41, 41, 41, 54, 54, 54, 71, 71, 91,
];

impl Default for ScalingList {
    /// The lists used when scaling lists are on but none is sent (7.4.5).
    fn default() -> Self {
        let pick = |m: usize| if m < 3 { DEFAULT_INTRA } else { DEFAULT_INTER };
        Self {
            s4: [[16; 16]; 6],
            s8: std::array::from_fn(pick),
            s16: std::array::from_fn(pick),
            s32: [DEFAULT_INTRA, DEFAULT_INTER],
            dc16: [16; 6],
            dc32: [16; 2],
        }
    }
}

impl ScalingList {
    fn list(&self, size_id: usize, m: usize) -> Vec<u8> {
        match size_id {
            0 => self.s4[m].to_vec(),
            1 => self.s8[m].to_vec(),
            2 => self.s16[m].to_vec(),
            _ => self.s32[m / 3].to_vec(),
        }
    }

    fn set(&mut self, size_id: usize, m: usize, v: &[u8], dc: u8) {
        match size_id {
            0 => self.s4[m].copy_from_slice(&v[..16]),
            1 => self.s8[m].copy_from_slice(&v[..64]),
            2 => {
                self.s16[m].copy_from_slice(&v[..64]);
                self.dc16[m] = dc;
            }
            _ => {
                self.s32[m / 3].copy_from_slice(&v[..64]);
                self.dc32[m / 3] = dc;
            }
        }
    }

    fn dc(&self, size_id: usize, m: usize) -> u8 {
        match size_id {
            2 => self.dc16[m],
            3 => self.dc32[m / 3],
            _ => 16,
        }
    }

    /// scaling_list_data() (7.3.4).
    fn read(b: &mut BitReader) -> Result<Self> {
        let mut sl = ScalingList::default();
        for size_id in 0..4usize {
            let step = if size_id == 3 { 3 } else { 1 };
            let mut m = 0usize;
            while m < 6 {
                let coded = r!(b, flag, "scaling_list_pred_mode_flag[{}][{}]", size_id, m);
                if !coded {
                    let delta = r!(b, ue, "scaling_list_pred_matrix_id_delta[{}][{}]", size_id, m) as usize;
                    if delta == 0 {
                        let d = ScalingList::default();
                        sl.set(size_id, m, &d.list(size_id, m), 16);
                    } else {
                        let rm = m.checked_sub(delta * step).ok_or(Error::Invalid("scaling list reference"))?;
                        let (v, dc) = (sl.list(size_id, rm), sl.dc(size_id, rm));
                        sl.set(size_id, m, &v, dc);
                    }
                } else {
                    let n = (1usize << (4 + (size_id << 1))).min(64);
                    let mut next = 8i32;
                    let mut dc = 16u8;
                    if size_id > 1 {
                        let d = r!(b, se, "scaling_list_dc_coef_minus8[{}][{}]", size_id - 2, m);
                        if !(-7..=247).contains(&d) {
                            return Err(Error::Invalid("scaling_list_dc_coef_minus8"));
                        }
                        next = d + 8;
                        dc = next as u8;
                    }
                    let mut v = vec![0u8; n];
                    for (i, x) in v.iter_mut().enumerate() {
                        let delta = r!(b, se, "scaling_list_delta_coeff[{}][{}][{}]", size_id, m, i);
                        if !(-128..=127).contains(&delta) {
                            return Err(Error::Invalid("scaling_list_delta_coef"));
                        }
                        next = (next + delta + 256).rem_euclid(256);
                        *x = next as u8;
                    }
                    sl.set(size_id, m, &v, dc);
                }
                m += step;
            }
        }
        Ok(sl)
    }
}

// ------------------------------------------------------------------ reference picture sets

/// One short-term reference picture set, derived (7.4.8): POC deltas of the pictures before
/// (S0, negative) and after (S1, positive) the current one, and which the current picture uses.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StRps {
    pub delta_poc_s0: Vec<i32>,
    pub used_s0: Vec<bool>,
    pub delta_poc_s1: Vec<i32>,
    pub used_s1: Vec<bool>,
}

impl StRps {
    pub fn num_negative(&self) -> usize {
        self.delta_poc_s0.len()
    }
    pub fn num_positive(&self) -> usize {
        self.delta_poc_s1.len()
    }
    pub fn num_delta_pocs(&self) -> usize {
        self.num_negative() + self.num_positive()
    }

    /// st_ref_pic_set(idx) (7.3.7). `sets` are the SPS's sets read so far; `num_sets` is the
    /// SPS's count (equal to `idx` when read from a slice header).
    pub fn read(b: &mut BitReader, idx: usize, num_sets: usize, sets: &[StRps]) -> Result<Self> {
        Ok(Self::read_with_ref(b, idx, num_sets, sets)?.0)
    }

    /// Like [`StRps::read`], also giving NumDeltaPocs of the set this one was predicted from
    /// (0 when it is coded directly): the GPU wants it for sets sent in a slice header.
    pub fn read_with_ref(b: &mut BitReader, idx: usize, num_sets: usize, sets: &[StRps]) -> Result<(Self, usize)> {
        let predicted = idx != 0 && r!(b, flag, "inter_ref_pic_set_prediction_flag");
        if predicted {
            let delta_idx = if idx == num_sets { r!(b, ue, "delta_idx_minus1") as usize + 1 } else { 1 };
            let ref_idx = idx.checked_sub(delta_idx).ok_or(Error::Invalid("delta_idx_minus1"))?;
            let rs = sets.get(ref_idx).ok_or(Error::Invalid("reference RPS"))?;
            let sign = r!(b, flag, "delta_rps_sign");
            let abs = r!(b, ue, "abs_delta_rps_minus1") as i32 + 1;
            if abs > 1 << 15 {
                return Err(Error::Invalid("abs_delta_rps_minus1"));
            }
            let delta_rps = if sign { -abs } else { abs };
            let n = rs.num_delta_pocs();
            let mut used = vec![false; n + 1];
            let mut use_delta = vec![true; n + 1];
            for j in 0..=n {
                used[j] = r!(b, flag, "used_by_curr_pic_flag[{}]", j);
                if !used[j] {
                    use_delta[j] = r!(b, flag, "use_delta_flag[{}]", j);
                }
            }
            let nn = rs.num_negative();
            let np = rs.num_positive();
            let mut out = StRps::default();
            for j in (0..np).rev() {
                let d = rs.delta_poc_s1[j] + delta_rps;
                if d < 0 && use_delta[nn + j] {
                    out.delta_poc_s0.push(d);
                    out.used_s0.push(used[nn + j]);
                }
            }
            if delta_rps < 0 && use_delta[n] {
                out.delta_poc_s0.push(delta_rps);
                out.used_s0.push(used[n]);
            }
            for j in 0..nn {
                let d = rs.delta_poc_s0[j] + delta_rps;
                if d < 0 && use_delta[j] {
                    out.delta_poc_s0.push(d);
                    out.used_s0.push(used[j]);
                }
            }
            for j in (0..nn).rev() {
                let d = rs.delta_poc_s0[j] + delta_rps;
                if d > 0 && use_delta[j] {
                    out.delta_poc_s1.push(d);
                    out.used_s1.push(used[j]);
                }
            }
            if delta_rps > 0 && use_delta[n] {
                out.delta_poc_s1.push(delta_rps);
                out.used_s1.push(used[n]);
            }
            for j in 0..np {
                let d = rs.delta_poc_s1[j] + delta_rps;
                if d > 0 && use_delta[nn + j] {
                    out.delta_poc_s1.push(d);
                    out.used_s1.push(used[nn + j]);
                }
            }
            if out.num_delta_pocs() > 16 {
                return Err(Error::Invalid("too many reference pictures"));
            }
            Ok((out, n))
        } else {
            let nn = r!(b, ue, "num_negative_pics") as usize;
            let np = r!(b, ue, "num_positive_pics") as usize;
            if nn > 16 || np > 16 || nn + np > 16 {
                return Err(Error::Invalid("too many reference pictures"));
            }
            let mut out = StRps::default();
            let mut poc = 0i32;
            for i in 0..nn {
                let d = r!(b, ue, "delta_poc_s0_minus1[{}]", i) as i32 + 1;
                poc -= d;
                out.delta_poc_s0.push(poc);
                out.used_s0.push(r!(b, flag, "used_by_curr_pic_s0_flag[{}]", i));
            }
            poc = 0;
            for i in 0..np {
                let d = r!(b, ue, "delta_poc_s1_minus1[{}]", i) as i32 + 1;
                poc += d;
                out.delta_poc_s1.push(poc);
                out.used_s1.push(r!(b, flag, "used_by_curr_pic_s1_flag[{}]", i));
            }
            Ok((out, 0))
        }
    }
}

// ------------------------------------------------------------------ VPS / SPS

/// profile_tier_level() (7.3.3): the general profile and level; sub-layer details are skipped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Profile {
    pub profile_idc: u8,
    pub tier: bool,
    pub level_idc: u8,
    pub compatibility: u32,
}

fn profile_tier_level(b: &mut BitReader, max_sub_layers_minus1: u8) -> Result<Profile> {
    let _space = r!(b, u 2, "general_profile_space");
    let tier = r!(b, flag, "general_tier_flag");
    let profile_idc = r!(b, u 5, "general_profile_idc") as u8;
    let mut compatibility = 0u32;
    for j in 0..32 {
        if r!(b, flag, "general_profile_compatibility_flag[{}]", j) {
            compatibility |= 1 << j;
        }
    }
    r!(b, flag, "general_progressive_source_flag");
    r!(b, flag, "general_interlaced_source_flag");
    r!(b, flag, "general_non_packed_constraint_flag");
    r!(b, flag, "general_frame_only_constraint_flag");
    // 43 bits of profile-specific constraint flags, then general_inbld_flag / reserved bit
    b.skip(44)?;
    let level_idc = r!(b, u 8, "general_level_idc") as u8;
    let n = max_sub_layers_minus1 as usize;
    let mut profile_present = [false; 8];
    let mut level_present = [false; 8];
    for i in 0..n {
        profile_present[i] = r!(b, flag, "sub_layer_profile_present_flag[{}]", i);
        level_present[i] = r!(b, flag, "sub_layer_level_present_flag[{}]", i);
    }
    if n > 0 {
        b.skip(2 * (8 - n))?;
    }
    for i in 0..n {
        if profile_present[i] {
            b.skip(88)?;
        }
        if level_present[i] {
            b.skip(8)?;
        }
    }
    Ok(Profile { profile_idc, tier, level_idc, compatibility })
}

/// The parts of the video parameter set a decoder needs (just its id: the rest repeats the SPS).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vps {
    pub id: u8,
}

pub fn vps(b: &mut BitReader) -> Result<Vps> {
    let id = r!(b, u 4, "vps_video_parameter_set_id") as u8;
    Ok(Vps { id })
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Vui {
    pub sar: (u16, u16),
    pub full_range: bool,
    pub colour_primaries: u8,
    pub transfer_characteristics: u8,
    pub matrix_coefficients: u8,
    /// (num_units_in_tick, time_scale)
    pub timing: Option<(u32, u32)>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sps {
    pub id: u8,
    pub vps_id: u8,
    pub max_sub_layers_minus1: u8,
    pub profile: Profile,
    pub chroma_format_idc: u8,
    pub separate_colour_plane: bool,
    pub width: u32,
    pub height: u32,
    /// Conformance window in luma samples: (left, right, top, bottom).
    pub crop: (u32, u32, u32, u32),
    pub bit_depth_luma: u8,
    pub bit_depth_chroma: u8,
    pub log2_max_poc_lsb: u8,
    pub max_dec_pic_buffering_minus1: u8,
    pub max_num_reorder_pics: u8,
    pub max_latency_increase_plus1: u32,
    pub log2_min_cb_size: u8,
    pub log2_diff_max_min_cb_size: u8,
    pub log2_min_tb_size: u8,
    pub log2_diff_max_min_tb_size: u8,
    pub max_transform_hierarchy_depth_inter: u8,
    pub max_transform_hierarchy_depth_intra: u8,
    pub scaling_list_enabled: bool,
    /// The SPS's own lists (`None` with scaling lists on: the defaults, unless the PPS sends some).
    pub scaling_list: Option<ScalingList>,
    pub amp_enabled: bool,
    pub sao_enabled: bool,
    pub pcm: Option<Pcm>,
    pub st_rps: Vec<StRps>,
    pub long_term_refs_present: bool,
    /// (lt_ref_pic_poc_lsb_sps, used_by_curr_pic_lt_sps_flag)
    pub lt_ref_pics: Vec<(u32, bool)>,
    pub temporal_mvp_enabled: bool,
    pub strong_intra_smoothing: bool,
    pub vui: Vui,
    pub range_extension: RangeExtension,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pcm {
    pub bit_depth_luma: u8,
    pub bit_depth_chroma: u8,
    pub log2_min_size: u8,
    pub log2_diff_max_min_size: u8,
    pub loop_filter_disabled: bool,
}

/// sps_range_extension() flags (4:2:2 / 4:4:4 / high bit depth profiles).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RangeExtension {
    pub transform_skip_rotation: bool,
    pub transform_skip_context: bool,
    pub implicit_rdpcm: bool,
    pub explicit_rdpcm: bool,
    pub extended_precision_processing: bool,
    pub intra_smoothing_disabled: bool,
    pub high_precision_offsets: bool,
    pub persistent_rice_adaptation: bool,
    pub cabac_bypass_alignment: bool,
}

impl Sps {
    /// The picture size after the conformance window.
    pub fn display_size(&self) -> (u32, u32) {
        let (l, r, t, b) = self.crop;
        (self.width.saturating_sub(l + r), self.height.saturating_sub(t + b))
    }
    pub fn log2_ctb_size(&self) -> u8 {
        self.log2_min_cb_size + self.log2_diff_max_min_cb_size
    }
}

fn hrd_parameters(b: &mut BitReader, common: bool, max_sub_layers_minus1: u8) -> Result<()> {
    let (mut nal, mut vcl, mut sub_pic) = (false, false, false);
    if common {
        nal = r!(b, flag, "nal_hrd_parameters_present_flag");
        vcl = r!(b, flag, "vcl_hrd_parameters_present_flag");
        if nal || vcl {
            sub_pic = r!(b, flag, "sub_pic_hrd_params_present_flag");
            if sub_pic {
                r!(b, u 8, "tick_divisor_minus2");
                r!(b, u 5, "du_cpb_removal_delay_increment_length_minus1");
                r!(b, flag, "sub_pic_cpb_params_in_pic_timing_sei_flag");
                r!(b, u 5, "dpb_output_delay_du_length_minus1");
            }
            r!(b, u 4, "bit_rate_scale");
            r!(b, u 4, "cpb_size_scale");
            if sub_pic {
                r!(b, u 4, "cpb_size_du_scale");
            }
            r!(b, u 5, "initial_cpb_removal_delay_length_minus1");
            r!(b, u 5, "au_cpb_removal_delay_length_minus1");
            r!(b, u 5, "dpb_output_delay_length_minus1");
        }
    }
    for i in 0..=max_sub_layers_minus1 as usize {
        let general = r!(b, flag, "fixed_pic_rate_general_flag[{}]", i);
        let within = if !general { r!(b, flag, "fixed_pic_rate_within_cvs_flag[{}]", i) } else { true };
        let mut low_delay = false;
        if within {
            r!(b, ue, "elemental_duration_in_tc_minus1[{}]", i);
        } else {
            low_delay = r!(b, flag, "low_delay_hrd_flag[{}]", i);
        }
        let cpb_cnt = if !low_delay { r!(b, ue, "cpb_cnt_minus1[{}]", i) as usize } else { 0 };
        if cpb_cnt > 31 {
            return Err(Error::Invalid("cpb_cnt_minus1"));
        }
        for present in [nal, vcl] {
            if present {
                for j in 0..=cpb_cnt {
                    r!(b, ue, "bit_rate_value_minus1[{}]", j);
                    r!(b, ue, "cpb_size_value_minus1[{}]", j);
                    if sub_pic {
                        r!(b, ue, "cpb_size_du_value_minus1[{}]", j);
                        r!(b, ue, "bit_rate_du_value_minus1[{}]", j);
                    }
                    r!(b, flag, "cbr_flag[{}]", j);
                }
            }
        }
    }
    Ok(())
}

fn vui(b: &mut BitReader, max_sub_layers_minus1: u8) -> Result<Vui> {
    let mut v = Vui { sar: (1, 1), colour_primaries: 2, transfer_characteristics: 2, matrix_coefficients: 2, ..Default::default() };
    if r!(b, flag, "aspect_ratio_info_present_flag") {
        let idc = r!(b, u 8, "aspect_ratio_idc");
        const SAR: [(u16, u16); 17] =
            [(0, 0), (1, 1), (12, 11), (10, 11), (16, 11), (40, 33), (24, 11), (20, 11), (32, 11), (80, 33), (18, 11), (15, 11), (64, 33), (160, 99), (4, 3), (3, 2), (2, 1)];
        if idc == 255 {
            let w = r!(b, u 16, "sar_width") as u16;
            let h = r!(b, u 16, "sar_height") as u16;
            v.sar = (w, h);
        } else if let Some(&s) = SAR.get(idc as usize) {
            v.sar = s;
        }
    }
    if r!(b, flag, "overscan_info_present_flag") {
        r!(b, flag, "overscan_appropriate_flag");
    }
    if r!(b, flag, "video_signal_type_present_flag") {
        r!(b, u 3, "video_format");
        v.full_range = r!(b, flag, "video_full_range_flag");
        if r!(b, flag, "colour_description_present_flag") {
            v.colour_primaries = r!(b, u 8, "colour_primaries") as u8;
            v.transfer_characteristics = r!(b, u 8, "transfer_characteristics") as u8;
            v.matrix_coefficients = r!(b, u 8, "matrix_coefficients") as u8;
        }
    }
    if r!(b, flag, "chroma_loc_info_present_flag") {
        r!(b, ue, "chroma_sample_loc_type_top_field");
        r!(b, ue, "chroma_sample_loc_type_bottom_field");
    }
    r!(b, flag, "neutral_chroma_indication_flag");
    r!(b, flag, "field_seq_flag");
    r!(b, flag, "frame_field_info_present_flag");
    if r!(b, flag, "default_display_window_flag") {
        r!(b, ue, "def_disp_win_left_offset");
        r!(b, ue, "def_disp_win_right_offset");
        r!(b, ue, "def_disp_win_top_offset");
        r!(b, ue, "def_disp_win_bottom_offset");
    }
    if r!(b, flag, "vui_timing_info_present_flag") {
        let n = r!(b, u 32, "vui_num_units_in_tick");
        let t = r!(b, u 32, "vui_time_scale");
        v.timing = Some((n, t));
        if r!(b, flag, "vui_poc_proportional_to_timing_flag") {
            r!(b, ue, "vui_num_ticks_poc_diff_one_minus1");
        }
        if r!(b, flag, "vui_hrd_parameters_present_flag") {
            hrd_parameters(b, true, max_sub_layers_minus1)?;
        }
    }
    if r!(b, flag, "bitstream_restriction_flag") {
        r!(b, flag, "tiles_fixed_structure_flag");
        r!(b, flag, "motion_vectors_over_pic_boundaries_flag");
        r!(b, flag, "restricted_ref_pic_lists_flag");
        r!(b, ue, "min_spatial_segmentation_idc");
        r!(b, ue, "max_bytes_per_pic_denom");
        r!(b, ue, "max_bits_per_min_cu_denom");
        r!(b, ue, "log2_max_mv_length_horizontal");
        r!(b, ue, "log2_max_mv_length_vertical");
    }
    Ok(v)
}

/// seq_parameter_set_rbsp() (7.3.2.2), after the NAL header.
pub fn sps(b: &mut BitReader) -> Result<Sps> {
    let vps_id = r!(b, u 4, "sps_video_parameter_set_id") as u8;
    let max_sub_layers_minus1 = r!(b, u 3, "sps_max_sub_layers_minus1") as u8;
    if max_sub_layers_minus1 > 6 {
        return Err(Error::Invalid("sps_max_sub_layers_minus1"));
    }
    r!(b, flag, "sps_temporal_id_nesting_flag");
    let profile = profile_tier_level(b, max_sub_layers_minus1)?;
    let id = r!(b, ue, "sps_seq_parameter_set_id");
    if id > 15 {
        return Err(Error::Invalid("sps_seq_parameter_set_id"));
    }
    let chroma_format_idc = r!(b, ue, "chroma_format_idc");
    if chroma_format_idc > 3 {
        return Err(Error::Invalid("chroma_format_idc"));
    }
    let separate_colour_plane = chroma_format_idc == 3 && r!(b, flag, "separate_colour_plane_flag");
    let width = r!(b, ue, "pic_width_in_luma_samples");
    let height = r!(b, ue, "pic_height_in_luma_samples");
    if width == 0 || height == 0 || width > 16888 || height > 16888 {
        return Err(Error::Invalid("picture size"));
    }
    let mut crop = (0, 0, 0, 0);
    if r!(b, flag, "conformance_window_flag") {
        // offsets count in chroma samples
        let (sx, sy) = match chroma_format_idc {
            1 => (2, 2),
            2 => (2, 1),
            _ => (1, 1),
        };
        crop.0 = r!(b, ue, "conf_win_left_offset") * sx;
        crop.1 = r!(b, ue, "conf_win_right_offset") * sx;
        crop.2 = r!(b, ue, "conf_win_top_offset") * sy;
        crop.3 = r!(b, ue, "conf_win_bottom_offset") * sy;
    }
    let bit_depth_luma = r!(b, ue, "bit_depth_luma_minus8") + 8;
    let bit_depth_chroma = r!(b, ue, "bit_depth_chroma_minus8") + 8;
    if bit_depth_luma > 16 || bit_depth_chroma > 16 {
        return Err(Error::Invalid("bit depth"));
    }
    let log2_max_poc_lsb = r!(b, ue, "log2_max_pic_order_cnt_lsb_minus4") + 4;
    if log2_max_poc_lsb > 16 {
        return Err(Error::Invalid("log2_max_pic_order_cnt_lsb_minus4"));
    }
    let ordering = r!(b, flag, "sps_sub_layer_ordering_info_present_flag");
    let (mut dpb, mut reorder, mut latency) = (0, 0, 0);
    let first = if ordering { 0 } else { max_sub_layers_minus1 as usize };
    for i in first..=max_sub_layers_minus1 as usize {
        dpb = r!(b, ue, "sps_max_dec_pic_buffering_minus1[{}]", i);
        reorder = r!(b, ue, "sps_max_num_reorder_pics[{}]", i);
        latency = r!(b, ue, "sps_max_latency_increase_plus1[{}]", i);
    }
    if dpb > 15 || reorder > dpb {
        return Err(Error::Invalid("decoded picture buffer size"));
    }
    let log2_min_cb_size = r!(b, ue, "log2_min_luma_coding_block_size_minus3") + 3;
    let log2_diff_max_min_cb_size = r!(b, ue, "log2_diff_max_min_luma_coding_block_size");
    let log2_min_tb_size = r!(b, ue, "log2_min_luma_transform_block_size_minus2") + 2;
    let log2_diff_max_min_tb_size = r!(b, ue, "log2_diff_max_min_luma_transform_block_size");
    if log2_min_cb_size + log2_diff_max_min_cb_size > 6 || log2_min_tb_size + log2_diff_max_min_tb_size > 5 {
        return Err(Error::Invalid("block sizes"));
    }
    let max_transform_hierarchy_depth_inter = r!(b, ue, "max_transform_hierarchy_depth_inter") as u8;
    let max_transform_hierarchy_depth_intra = r!(b, ue, "max_transform_hierarchy_depth_intra") as u8;
    let scaling_list_enabled = r!(b, flag, "scaling_list_enabled_flag");
    let mut scaling_list = None;
    if scaling_list_enabled && r!(b, flag, "sps_scaling_list_data_present_flag") {
        scaling_list = Some(ScalingList::read(b)?);
    }
    let amp_enabled = r!(b, flag, "amp_enabled_flag");
    let sao_enabled = r!(b, flag, "sample_adaptive_offset_enabled_flag");
    let mut pcm = None;
    if r!(b, flag, "pcm_enabled_flag") {
        pcm = Some(Pcm {
            bit_depth_luma: r!(b, u 4, "pcm_sample_bit_depth_luma_minus1") as u8 + 1,
            bit_depth_chroma: r!(b, u 4, "pcm_sample_bit_depth_chroma_minus1") as u8 + 1,
            log2_min_size: r!(b, ue, "log2_min_pcm_luma_coding_block_size_minus3") as u8 + 3,
            log2_diff_max_min_size: r!(b, ue, "log2_diff_max_min_pcm_luma_coding_block_size") as u8,
            loop_filter_disabled: r!(b, flag, "pcm_loop_filter_disabled_flag"),
        });
    }
    let n = r!(b, ue, "num_short_term_ref_pic_sets") as usize;
    if n > 64 {
        return Err(Error::Invalid("num_short_term_ref_pic_sets"));
    }
    let mut st_rps = Vec::with_capacity(n);
    for i in 0..n {
        let s = StRps::read(b, i, n, &st_rps)?;
        st_rps.push(s);
    }
    let long_term_refs_present = r!(b, flag, "long_term_ref_pics_present_flag");
    let mut lt_ref_pics = Vec::new();
    if long_term_refs_present {
        let n = r!(b, ue, "num_long_term_ref_pics_sps") as usize;
        if n > 32 {
            return Err(Error::Invalid("num_long_term_ref_pics_sps"));
        }
        for i in 0..n {
            let lsb = r!(b, u log2_max_poc_lsb, "lt_ref_pic_poc_lsb_sps[{}]", i);
            let used = r!(b, flag, "used_by_curr_pic_lt_sps_flag[{}]", i);
            lt_ref_pics.push((lsb, used));
        }
    }
    let temporal_mvp_enabled = r!(b, flag, "sps_temporal_mvp_enabled_flag");
    let strong_intra_smoothing = r!(b, flag, "strong_intra_smoothing_enabled_flag");
    let vui = if r!(b, flag, "vui_parameters_present_flag") {
        vui(b, max_sub_layers_minus1)?
    } else {
        Vui { sar: (1, 1), colour_primaries: 2, transfer_characteristics: 2, matrix_coefficients: 2, ..Default::default() }
    };
    let mut range_extension = RangeExtension::default();
    if r!(b, flag, "sps_extension_present_flag") {
        let range = r!(b, flag, "sps_range_extension_flag");
        r!(b, flag, "sps_multilayer_extension_flag");
        r!(b, flag, "sps_3d_extension_flag");
        r!(b, flag, "sps_scc_extension_flag");
        r!(b, u 4, "sps_extension_4bits");
        if range {
            range_extension = RangeExtension {
                transform_skip_rotation: r!(b, flag, "transform_skip_rotation_enabled_flag"),
                transform_skip_context: r!(b, flag, "transform_skip_context_enabled_flag"),
                implicit_rdpcm: r!(b, flag, "implicit_rdpcm_enabled_flag"),
                explicit_rdpcm: r!(b, flag, "explicit_rdpcm_enabled_flag"),
                extended_precision_processing: r!(b, flag, "extended_precision_processing_flag"),
                intra_smoothing_disabled: r!(b, flag, "intra_smoothing_disabled_flag"),
                high_precision_offsets: r!(b, flag, "high_precision_offsets_enabled_flag"),
                persistent_rice_adaptation: r!(b, flag, "persistent_rice_adaptation_enabled_flag"),
                cabac_bypass_alignment: r!(b, flag, "cabac_bypass_alignment_enabled_flag"),
            };
        }
    }
    Ok(Sps {
        id: id as u8,
        vps_id,
        max_sub_layers_minus1,
        profile,
        chroma_format_idc: chroma_format_idc as u8,
        separate_colour_plane,
        width,
        height,
        crop,
        bit_depth_luma: bit_depth_luma as u8,
        bit_depth_chroma: bit_depth_chroma as u8,
        log2_max_poc_lsb: log2_max_poc_lsb as u8,
        max_dec_pic_buffering_minus1: dpb as u8,
        max_num_reorder_pics: reorder as u8,
        max_latency_increase_plus1: latency,
        log2_min_cb_size: log2_min_cb_size as u8,
        log2_diff_max_min_cb_size: log2_diff_max_min_cb_size as u8,
        log2_min_tb_size: log2_min_tb_size as u8,
        log2_diff_max_min_tb_size: log2_diff_max_min_tb_size as u8,
        max_transform_hierarchy_depth_inter,
        max_transform_hierarchy_depth_intra,
        scaling_list_enabled,
        scaling_list,
        amp_enabled,
        sao_enabled,
        pcm,
        st_rps,
        long_term_refs_present,
        lt_ref_pics,
        temporal_mvp_enabled,
        strong_intra_smoothing,
        vui,
        range_extension,
    })
}

// ------------------------------------------------------------------ PPS

#[derive(Debug, Clone, PartialEq)]
pub struct Pps {
    pub id: u8,
    pub sps_id: u8,
    pub dependent_slice_segments_enabled: bool,
    pub output_flag_present: bool,
    pub num_extra_slice_header_bits: u8,
    pub sign_data_hiding_enabled: bool,
    pub cabac_init_present: bool,
    pub num_ref_idx_l0_default_active_minus1: u8,
    pub num_ref_idx_l1_default_active_minus1: u8,
    pub init_qp_minus26: i8,
    pub constrained_intra_pred: bool,
    pub transform_skip_enabled: bool,
    pub cu_qp_delta_enabled: bool,
    pub diff_cu_qp_delta_depth: u8,
    pub cb_qp_offset: i8,
    pub cr_qp_offset: i8,
    pub slice_chroma_qp_offsets_present: bool,
    pub weighted_pred: bool,
    pub weighted_bipred: bool,
    pub transquant_bypass_enabled: bool,
    pub tiles_enabled: bool,
    pub entropy_coding_sync_enabled: bool,
    pub tiles: Option<Tiles>,
    pub loop_filter_across_slices: bool,
    pub deblocking_filter_control_present: bool,
    pub deblocking_filter_override_enabled: bool,
    pub deblocking_filter_disabled: bool,
    pub beta_offset_div2: i8,
    pub tc_offset_div2: i8,
    pub scaling_list: Option<ScalingList>,
    pub lists_modification_present: bool,
    pub log2_parallel_merge_level: u8,
    pub slice_segment_header_extension_present: bool,
    pub range_extension: Option<PpsRange>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tiles {
    pub columns: u16,
    pub rows: u16,
    pub uniform_spacing: bool,
    /// Explicit widths/heights in CTBs (all but the last), when not uniform.
    pub column_widths: Vec<u16>,
    pub row_heights: Vec<u16>,
    pub loop_filter_across_tiles: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PpsRange {
    pub log2_max_transform_skip_block_size: u8,
    pub cross_component_prediction: bool,
    pub chroma_qp_offset_list_enabled: bool,
    pub diff_cu_chroma_qp_offset_depth: u8,
    pub cb_qp_offset_list: Vec<i8>,
    pub cr_qp_offset_list: Vec<i8>,
    pub log2_sao_offset_scale_luma: u8,
    pub log2_sao_offset_scale_chroma: u8,
}

/// pic_parameter_set_rbsp() (7.3.2.3), after the NAL header.
pub fn pps(b: &mut BitReader) -> Result<Pps> {
    let id = r!(b, ue, "pps_pic_parameter_set_id");
    let sps_id = r!(b, ue, "pps_seq_parameter_set_id");
    if id > 63 || sps_id > 15 {
        return Err(Error::Invalid("parameter set id"));
    }
    let dependent_slice_segments_enabled = r!(b, flag, "dependent_slice_segments_enabled_flag");
    let output_flag_present = r!(b, flag, "output_flag_present_flag");
    let num_extra_slice_header_bits = r!(b, u 3, "num_extra_slice_header_bits") as u8;
    let sign_data_hiding_enabled = r!(b, flag, "sign_data_hiding_enabled_flag");
    let cabac_init_present = r!(b, flag, "cabac_init_present_flag");
    let l0 = r!(b, ue, "num_ref_idx_l0_default_active_minus1");
    let l1 = r!(b, ue, "num_ref_idx_l1_default_active_minus1");
    if l0 > 14 || l1 > 14 {
        return Err(Error::Invalid("num_ref_idx_default_active_minus1"));
    }
    let init_qp_minus26 = r!(b, se, "init_qp_minus26");
    if !(-(26 + 6 * 8)..=25).contains(&init_qp_minus26) {
        return Err(Error::Invalid("init_qp_minus26"));
    }
    let constrained_intra_pred = r!(b, flag, "constrained_intra_pred_flag");
    let transform_skip_enabled = r!(b, flag, "transform_skip_enabled_flag");
    let cu_qp_delta_enabled = r!(b, flag, "cu_qp_delta_enabled_flag");
    let diff_cu_qp_delta_depth = if cu_qp_delta_enabled { r!(b, ue, "diff_cu_qp_delta_depth") as u8 } else { 0 };
    let cb_qp_offset = r!(b, se, "pps_cb_qp_offset");
    let cr_qp_offset = r!(b, se, "pps_cr_qp_offset");
    if !(-12..=12).contains(&cb_qp_offset) || !(-12..=12).contains(&cr_qp_offset) {
        return Err(Error::Invalid("chroma QP offset"));
    }
    let slice_chroma_qp_offsets_present = r!(b, flag, "pps_slice_chroma_qp_offsets_present_flag");
    let weighted_pred = r!(b, flag, "weighted_pred_flag");
    let weighted_bipred = r!(b, flag, "weighted_bipred_flag");
    let transquant_bypass_enabled = r!(b, flag, "transquant_bypass_enabled_flag");
    let tiles_enabled = r!(b, flag, "tiles_enabled_flag");
    let entropy_coding_sync_enabled = r!(b, flag, "entropy_coding_sync_enabled_flag");
    let mut tiles = None;
    if tiles_enabled {
        let cols = r!(b, ue, "num_tile_columns_minus1") as usize + 1;
        let rows = r!(b, ue, "num_tile_rows_minus1") as usize + 1;
        if cols > 20 || rows > 22 {
            return Err(Error::Invalid("tile count"));
        }
        let uniform_spacing = r!(b, flag, "uniform_spacing_flag");
        let (mut column_widths, mut row_heights) = (Vec::new(), Vec::new());
        if !uniform_spacing {
            for i in 0..cols - 1 {
                column_widths.push(r!(b, ue, "column_width_minus1[{}]", i) as u16 + 1);
            }
            for i in 0..rows - 1 {
                row_heights.push(r!(b, ue, "row_height_minus1[{}]", i) as u16 + 1);
            }
        }
        let loop_filter_across_tiles = r!(b, flag, "loop_filter_across_tiles_enabled_flag");
        tiles = Some(Tiles { columns: cols as u16, rows: rows as u16, uniform_spacing, column_widths, row_heights, loop_filter_across_tiles });
    }
    let loop_filter_across_slices = r!(b, flag, "pps_loop_filter_across_slices_enabled_flag");
    let deblocking_filter_control_present = r!(b, flag, "deblocking_filter_control_present_flag");
    let (mut deblocking_filter_override_enabled, mut deblocking_filter_disabled, mut beta_offset_div2, mut tc_offset_div2) = (false, false, 0, 0);
    if deblocking_filter_control_present {
        deblocking_filter_override_enabled = r!(b, flag, "deblocking_filter_override_enabled_flag");
        deblocking_filter_disabled = r!(b, flag, "pps_deblocking_filter_disabled_flag");
        if !deblocking_filter_disabled {
            beta_offset_div2 = r!(b, se, "pps_beta_offset_div2");
            tc_offset_div2 = r!(b, se, "pps_tc_offset_div2");
            if !(-6..=6).contains(&beta_offset_div2) || !(-6..=6).contains(&tc_offset_div2) {
                return Err(Error::Invalid("deblocking offsets"));
            }
        }
    }
    let scaling_list = if r!(b, flag, "pps_scaling_list_data_present_flag") { Some(ScalingList::read(b)?) } else { None };
    let lists_modification_present = r!(b, flag, "lists_modification_present_flag");
    let log2_parallel_merge_level = r!(b, ue, "log2_parallel_merge_level_minus2") + 2;
    if log2_parallel_merge_level > 6 {
        return Err(Error::Invalid("log2_parallel_merge_level_minus2"));
    }
    let slice_segment_header_extension_present = r!(b, flag, "slice_segment_header_extension_present_flag");
    let mut range_extension = None;
    if r!(b, flag, "pps_extension_present_flag") {
        let range = r!(b, flag, "pps_range_extension_flag");
        r!(b, flag, "pps_multilayer_extension_flag");
        r!(b, flag, "pps_3d_extension_flag");
        r!(b, flag, "pps_scc_extension_flag");
        r!(b, u 4, "pps_extension_4bits");
        if range {
            let log2_max_transform_skip_block_size = if transform_skip_enabled { r!(b, ue, "log2_max_transform_skip_block_size_minus2") as u8 + 2 } else { 2 };
            let cross_component_prediction = r!(b, flag, "cross_component_prediction_enabled_flag");
            let chroma_qp_offset_list_enabled = r!(b, flag, "chroma_qp_offset_list_enabled_flag");
            let (mut depth, mut cb, mut cr) = (0, Vec::new(), Vec::new());
            if chroma_qp_offset_list_enabled {
                depth = r!(b, ue, "diff_cu_chroma_qp_offset_depth") as u8;
                let n = r!(b, ue, "chroma_qp_offset_list_len_minus1") as usize + 1;
                if n > 6 {
                    return Err(Error::Invalid("chroma_qp_offset_list_len_minus1"));
                }
                for i in 0..n {
                    cb.push(r!(b, se, "cb_qp_offset_list[{}]", i) as i8);
                    cr.push(r!(b, se, "cr_qp_offset_list[{}]", i) as i8);
                }
            }
            range_extension = Some(PpsRange {
                log2_max_transform_skip_block_size,
                cross_component_prediction,
                chroma_qp_offset_list_enabled,
                diff_cu_chroma_qp_offset_depth: depth,
                cb_qp_offset_list: cb,
                cr_qp_offset_list: cr,
                log2_sao_offset_scale_luma: r!(b, ue, "log2_sao_offset_scale_luma") as u8,
                log2_sao_offset_scale_chroma: r!(b, ue, "log2_sao_offset_scale_chroma") as u8,
            });
        }
    }
    Ok(Pps {
        id: id as u8,
        sps_id: sps_id as u8,
        dependent_slice_segments_enabled,
        output_flag_present,
        num_extra_slice_header_bits,
        sign_data_hiding_enabled,
        cabac_init_present,
        num_ref_idx_l0_default_active_minus1: l0 as u8,
        num_ref_idx_l1_default_active_minus1: l1 as u8,
        init_qp_minus26: init_qp_minus26 as i8,
        constrained_intra_pred,
        transform_skip_enabled,
        cu_qp_delta_enabled,
        diff_cu_qp_delta_depth,
        cb_qp_offset: cb_qp_offset as i8,
        cr_qp_offset: cr_qp_offset as i8,
        slice_chroma_qp_offsets_present,
        weighted_pred,
        weighted_bipred,
        transquant_bypass_enabled,
        tiles_enabled,
        entropy_coding_sync_enabled,
        tiles,
        loop_filter_across_slices,
        deblocking_filter_control_present,
        deblocking_filter_override_enabled,
        deblocking_filter_disabled,
        beta_offset_div2: beta_offset_div2 as i8,
        tc_offset_div2: tc_offset_div2 as i8,
        scaling_list,
        lists_modification_present,
        log2_parallel_merge_level: log2_parallel_merge_level as u8,
        slice_segment_header_extension_present,
        range_extension,
    })
}
