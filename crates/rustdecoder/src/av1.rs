//! AV1 bitstream parts the host has to read for the GPU: OBU framing, the sequence header and
//! the uncompressed frame header (AV1 bitstream specification, 5.3, 5.5, 5.9). Everything inside
//! the tiles is entropy coded and left to the video engine.
//!
//! The frame header carries state from frame to frame, kept per reference slot (what a frame
//! stores when it refreshes a slot, spec 7.20): order hints, sizes, global motion parameters, loop
//! filter deltas, segmentation features and film grain parameters. [`Parser`] keeps those; the
//! decoder calls [`Parser::finish`] after every frame so the slots it replaced are updated. The
//! CDFs and motion vectors of a slot stay on the GPU.

use crate::bits::BitReader;
use crate::{Error, Result};

pub const OBU_SEQUENCE_HEADER: u8 = 1;
pub const OBU_TEMPORAL_DELIMITER: u8 = 2;
pub const OBU_FRAME_HEADER: u8 = 3;
pub const OBU_TILE_GROUP: u8 = 4;
pub const OBU_FRAME: u8 = 6;
pub const OBU_REDUNDANT_FRAME_HEADER: u8 = 7;

pub const KEY_FRAME: u8 = 0;
pub const INTER_FRAME: u8 = 1;
pub const INTRA_ONLY_FRAME: u8 = 2;
pub const SWITCH_FRAME: u8 = 3;

const PRIMARY_REF_NONE: u8 = 7;
const SELECT: u8 = 2;
const WARP_PREC: i32 = 16;

/// One OBU of a temporal unit.
#[derive(Debug, Clone, Copy)]
pub struct Obu<'a> {
    pub kind: u8,
    /// `Some((temporal_id, spatial_id))` when the header has the extension.
    pub layer: Option<(u8, u8)>,
    pub payload: &'a [u8],
}

fn leb128(d: &[u8], pos: &mut usize) -> Result<u64> {
    let mut v = 0u64;
    for i in 0..8 {
        let b = *d.get(*pos).ok_or(Error::Invalid("AV1 leb128 cut short"))?;
        *pos += 1;
        v |= ((b & 0x7f) as u64) << (7 * i);
        if b & 0x80 == 0 {
            return Ok(v);
        }
    }
    Err(Error::Invalid("AV1 leb128 too long"))
}

/// Splits a temporal unit (or a `configOBUs` blob) into OBUs.
pub fn split_obus(d: &[u8]) -> Result<Vec<Obu<'_>>> {
    let mut out = Vec::new();
    let mut p = 0usize;
    while p < d.len() {
        let h = d[p];
        p += 1;
        if h & 0x80 != 0 {
            return Err(Error::Invalid("AV1 OBU forbidden bit set"));
        }
        let kind = (h >> 3) & 15;
        let ext = h & 4 != 0;
        let has_size = h & 2 != 0;
        let mut layer = None;
        if ext {
            let e = *d.get(p).ok_or(Error::Invalid("AV1 OBU extension cut short"))?;
            p += 1;
            layer = Some((e >> 5, (e >> 3) & 3));
        }
        let size = if has_size { leb128(d, &mut p)? as usize } else { d.len() - p };
        if size > d.len() - p {
            return Err(Error::Invalid("AV1 OBU runs past the packet"));
        }
        out.push(Obu { kind, layer, payload: &d[p..p + size] });
        p += size;
    }
    Ok(out)
}

#[derive(Debug, Clone, Default)]
pub struct Sequence {
    pub profile: u8,
    pub still_picture: bool,
    pub reduced_still_picture_header: bool,
    /// `operating_point_idc[0]`.
    pub operating_point_idc: u16,
    pub decoder_model_info_present: bool,
    pub equal_picture_interval: bool,
    pub buffer_removal_time_length: u32,
    pub frame_presentation_time_length: u32,
    pub decoder_model_present_for_op: Vec<bool>,
    pub operating_point_idcs: Vec<u16>,
    pub frame_width_bits: u32,
    pub frame_height_bits: u32,
    pub max_frame_width: u32,
    pub max_frame_height: u32,
    pub frame_id_numbers_present: bool,
    pub delta_frame_id_length: u32,
    pub additional_frame_id_length: u32,
    pub use_128x128_superblock: bool,
    pub enable_filter_intra: bool,
    pub enable_intra_edge_filter: bool,
    pub enable_interintra_compound: bool,
    pub enable_masked_compound: bool,
    pub enable_warped_motion: bool,
    pub enable_dual_filter: bool,
    pub enable_order_hint: bool,
    pub enable_jnt_comp: bool,
    pub enable_ref_frame_mvs: bool,
    pub seq_force_screen_content_tools: u8,
    pub seq_force_integer_mv: u8,
    pub order_hint_bits: u32,
    pub enable_superres: bool,
    pub enable_cdef: bool,
    pub enable_restoration: bool,
    pub bit_depth: u8,
    pub mono_chrome: bool,
    pub color_primaries: u8,
    pub transfer_characteristics: u8,
    pub matrix_coefficients: u8,
    pub full_range: bool,
    pub subsampling_x: bool,
    pub subsampling_y: bool,
    pub separate_uv_delta_q: bool,
    pub film_grain_params_present: bool,
}

impl Sequence {
    pub fn num_planes(&self) -> usize {
        if self.mono_chrome { 1 } else { 3 }
    }
}

fn uvlc(r: &mut BitReader) -> Result<u32> {
    let mut zeros = 0;
    while !r.flag()? {
        zeros += 1;
        if zeros >= 32 {
            return Ok(u32::MAX);
        }
    }
    Ok(r.bits(zeros)? + ((1u64 << zeros) - 1) as u32)
}

/// `sequence_header_obu` (5.5).
pub fn parse_sequence(payload: &[u8]) -> Result<Sequence> {
    let mut r = BitReader::new(payload);
    let mut s = Sequence { profile: r.bits(3)? as u8, ..Default::default() };
    s.still_picture = r.flag()?;
    s.reduced_still_picture_header = r.flag()?;
    if s.reduced_still_picture_header {
        r.skip(5)?; // seq_level_idx[0]
        s.operating_point_idcs = vec![0];
        s.decoder_model_present_for_op = vec![false];
    } else {
        let timing_info_present = r.flag()?;
        let mut buffer_delay_length = 0;
        if timing_info_present {
            r.skip(64)?;
            s.equal_picture_interval = r.flag()?;
            if s.equal_picture_interval {
                uvlc(&mut r)?;
            }
            s.decoder_model_info_present = r.flag()?;
            if s.decoder_model_info_present {
                buffer_delay_length = r.bits(5)? + 1;
                r.skip(32)?;
                s.buffer_removal_time_length = r.bits(5)? + 1;
                s.frame_presentation_time_length = r.bits(5)? + 1;
            }
        }
        let initial_display_delay_present = r.flag()?;
        let cnt = r.bits(5)? + 1;
        for _ in 0..cnt {
            s.operating_point_idcs.push(r.bits(12)? as u16);
            let level = r.bits(5)?;
            if level > 7 {
                r.skip(1)?;
            }
            let mut present = false;
            if s.decoder_model_info_present {
                present = r.flag()?;
                if present {
                    r.skip(2 * buffer_delay_length as usize + 1)?;
                }
            }
            s.decoder_model_present_for_op.push(present);
            if initial_display_delay_present && r.flag()? {
                r.skip(4)?;
            }
        }
    }
    s.operating_point_idc = s.operating_point_idcs[0];
    s.frame_width_bits = r.bits(4)? + 1;
    s.frame_height_bits = r.bits(4)? + 1;
    s.max_frame_width = r.bits(s.frame_width_bits)? + 1;
    s.max_frame_height = r.bits(s.frame_height_bits)? + 1;
    if !s.reduced_still_picture_header {
        s.frame_id_numbers_present = r.flag()?;
    }
    if s.frame_id_numbers_present {
        s.delta_frame_id_length = r.bits(4)? + 2;
        s.additional_frame_id_length = r.bits(3)? + 1;
    }
    s.use_128x128_superblock = r.flag()?;
    s.enable_filter_intra = r.flag()?;
    s.enable_intra_edge_filter = r.flag()?;
    s.seq_force_screen_content_tools = SELECT;
    s.seq_force_integer_mv = SELECT;
    if !s.reduced_still_picture_header {
        s.enable_interintra_compound = r.flag()?;
        s.enable_masked_compound = r.flag()?;
        s.enable_warped_motion = r.flag()?;
        s.enable_dual_filter = r.flag()?;
        s.enable_order_hint = r.flag()?;
        if s.enable_order_hint {
            s.enable_jnt_comp = r.flag()?;
            s.enable_ref_frame_mvs = r.flag()?;
        }
        if !r.flag()? {
            s.seq_force_screen_content_tools = r.bits(1)? as u8;
        }
        if s.seq_force_screen_content_tools > 0 {
            if !r.flag()? {
                s.seq_force_integer_mv = r.bits(1)? as u8;
            }
        }
        if s.enable_order_hint {
            s.order_hint_bits = r.bits(3)? + 1;
        }
    }
    s.enable_superres = r.flag()?;
    s.enable_cdef = r.flag()?;
    s.enable_restoration = r.flag()?;
    // color_config
    let high = r.flag()?;
    s.bit_depth = if s.profile == 2 && high {
        if r.flag()? { 12 } else { 10 }
    } else if high {
        10
    } else {
        8
    };
    s.mono_chrome = if s.profile == 1 { false } else { r.flag()? };
    s.color_primaries = 2;
    s.transfer_characteristics = 2;
    s.matrix_coefficients = 2;
    if r.flag()? {
        s.color_primaries = r.bits(8)? as u8;
        s.transfer_characteristics = r.bits(8)? as u8;
        s.matrix_coefficients = r.bits(8)? as u8;
    }
    if s.mono_chrome {
        s.full_range = r.flag()?;
        s.subsampling_x = true;
        s.subsampling_y = true;
    } else if s.color_primaries == 1 && s.transfer_characteristics == 13 && s.matrix_coefficients == 0 {
        s.full_range = true;
    } else {
        s.full_range = r.flag()?;
        match s.profile {
            0 => {
                s.subsampling_x = true;
                s.subsampling_y = true;
            }
            1 => {}
            _ => {
                if s.bit_depth == 12 {
                    s.subsampling_x = r.flag()?;
                    if s.subsampling_x {
                        s.subsampling_y = r.flag()?;
                    }
                } else {
                    s.subsampling_x = true;
                }
            }
        }
        if s.subsampling_x && s.subsampling_y {
            r.skip(2)?;
        }
        s.separate_uv_delta_q = r.flag()?;
    }
    s.film_grain_params_present = r.flag()?;
    Ok(s)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FilmGrain {
    pub apply_grain: bool,
    pub grain_seed: u16,
    pub scaling_points_y: Vec<(u8, u8)>,
    pub chroma_scaling_from_luma: bool,
    pub scaling_points_cb: Vec<(u8, u8)>,
    pub scaling_points_cr: Vec<(u8, u8)>,
    pub grain_scaling_minus_8: u8,
    pub ar_coeff_lag: u8,
    pub ar_coeffs_y_plus_128: Vec<u8>,
    pub ar_coeffs_cb_plus_128: Vec<u8>,
    pub ar_coeffs_cr_plus_128: Vec<u8>,
    pub ar_coeff_shift_minus_6: u8,
    pub grain_scale_shift: u8,
    pub cb_mult: u8,
    pub cb_luma_mult: u8,
    pub cb_offset: u16,
    pub cr_mult: u8,
    pub cr_luma_mult: u8,
    pub cr_offset: u16,
    pub overlap_flag: bool,
    pub clip_to_restricted_range: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segmentation {
    pub enabled: bool,
    pub update_map: bool,
    pub temporal_update: bool,
    pub update_data: bool,
    pub feature_enabled: [[bool; 8]; 8],
    pub feature_data: [[i16; 8]; 8],
}

impl Default for Segmentation {
    fn default() -> Self {
        Self { enabled: false, update_map: false, temporal_update: false, update_data: false, feature_enabled: [[false; 8]; 8], feature_data: [[0; 8]; 8] }
    }
}

/// What a frame leaves in a reference slot (7.20) besides the picture itself.
#[derive(Debug, Clone)]
pub struct SlotState {
    pub valid: bool,
    pub frame_id: u32,
    pub upscaled_width: u32,
    pub frame_width: u32,
    pub frame_height: u32,
    pub render_width: u32,
    pub render_height: u32,
    pub frame_type: u8,
    pub order_hint: u32,
    pub saved_order_hints: [u32; 8],
    pub gm_params: [[i32; 6]; 8],
    pub ref_deltas: [i8; 8],
    pub mode_deltas: [i8; 2],
    pub seg: Segmentation,
    pub film_grain: FilmGrain,
    pub showable: bool,
    pub bit_depth: u8,
}

fn identity_gm() -> [[i32; 6]; 8] {
    let mut g = [[0i32; 6]; 8];
    for r in g.iter_mut() {
        r[2] = 1 << WARP_PREC;
        r[5] = 1 << WARP_PREC;
    }
    g
}

const DEFAULT_REF_DELTAS: [i8; 8] = [1, 0, 0, 0, -1, 0, -1, -1];

impl Default for SlotState {
    fn default() -> Self {
        Self {
            valid: false,
            frame_id: 0,
            upscaled_width: 0,
            frame_width: 0,
            frame_height: 0,
            render_width: 0,
            render_height: 0,
            frame_type: 0,
            order_hint: 0,
            saved_order_hints: [0; 8],
            gm_params: identity_gm(),
            ref_deltas: DEFAULT_REF_DELTAS,
            mode_deltas: [0; 2],
            seg: Segmentation::default(),
            film_grain: FilmGrain::default(),
            showable: false,
            bit_depth: 8,
        }
    }
}

/// A parsed frame header.
#[derive(Debug, Clone)]
pub struct Header {
    pub show_existing_frame: bool,
    pub frame_to_show: u8,
    pub frame_type: u8,
    pub frame_is_intra: bool,
    pub show_frame: bool,
    pub showable_frame: bool,
    pub error_resilient_mode: bool,
    pub disable_cdf_update: bool,
    pub allow_screen_content_tools: bool,
    pub force_integer_mv: bool,
    pub frame_size_override_flag: bool,
    pub order_hint: u32,
    pub primary_ref_frame: u8,
    pub refresh_frame_flags: u8,
    pub ref_frame_idx: [u8; 7],
    pub allow_high_precision_mv: bool,
    pub use_ref_frame_mvs: bool,
    pub allow_intrabc: bool,
    pub is_motion_mode_switchable: bool,
    pub interpolation_filter: u8,
    pub disable_frame_end_update_cdf: bool,
    // size
    pub upscaled_width: u32,
    pub frame_width: u32,
    pub frame_height: u32,
    pub render_width: u32,
    pub render_height: u32,
    pub use_superres: bool,
    pub superres_denom: u32,
    pub mi_cols: u32,
    pub mi_rows: u32,
    // tiles
    pub tile_cols: u32,
    pub tile_rows: u32,
    pub tile_cols_log2: u32,
    pub tile_rows_log2: u32,
    /// Tile widths and heights in superblocks.
    pub tile_widths_sb: Vec<u32>,
    pub tile_heights_sb: Vec<u32>,
    pub context_update_tile_id: u32,
    pub tile_size_bytes: u32,
    // quantization
    pub base_q_idx: u8,
    pub delta_q_y_dc: i8,
    pub delta_q_u_dc: i8,
    pub delta_q_u_ac: i8,
    pub delta_q_v_dc: i8,
    pub delta_q_v_ac: i8,
    pub using_qmatrix: bool,
    pub qm_y: u8,
    pub qm_u: u8,
    pub qm_v: u8,
    pub seg: Segmentation,
    pub delta_q_present: bool,
    pub delta_q_res: u8,
    pub delta_lf_present: bool,
    pub delta_lf_res: u8,
    pub delta_lf_multi: bool,
    pub coded_lossless: bool,
    // loop filter
    pub loop_filter_level: [u8; 4],
    pub sharpness: u8,
    pub lf_delta_enabled: bool,
    pub lf_delta_update: bool,
    pub ref_deltas: [i8; 8],
    pub mode_deltas: [i8; 2],
    // cdef
    pub cdef_damping_minus_3: u8,
    pub cdef_bits: u8,
    pub cdef_y_pri: [u8; 8],
    pub cdef_y_sec: [u8; 8],
    pub cdef_uv_pri: [u8; 8],
    pub cdef_uv_sec: [u8; 8],
    // loop restoration: 0 none, 1 wiener, 2 sgrproj, 3 switchable
    pub lr_type: [u8; 3],
    pub lr_unit_shift: u8,
    pub lr_uv_shift: u8,
    pub uses_lr: bool,
    pub tx_mode: u8,
    pub reference_select: bool,
    pub skip_mode_present: bool,
    pub allow_warped_motion: bool,
    pub reduced_tx_set: bool,
    pub gm_type: [u8; 8],
    pub gm_params: [[i32; 6]; 8],
    pub film_grain: FilmGrain,
    /// Order hints of the seven references (index 1..=7), saved with the frame.
    pub order_hints: [u32; 8],
    /// Bytes the header took (up to and including the byte alignment): where a `OBU_FRAME`'s tile
    /// group starts.
    pub header_bytes: usize,
}

/// The state kept between frames.
#[derive(Clone)]
pub struct Parser {
    pub seq: Option<Sequence>,
    pub slots: [SlotState; 8],
    /// Header of the frame being read (between its header and its last tile group).
    pub temporal_id: u8,
    pub spatial_id: u8,
    current_frame_id: u32,
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

fn tile_log2(blk: u32, target: u32) -> u32 {
    let mut k = 0;
    while (blk << k) < target {
        k += 1;
    }
    k
}

fn ns(r: &mut BitReader, n: u32) -> Result<u32> {
    let w = 32 - n.leading_zeros(); // FloorLog2(n) + 1
    let m = (1u32 << w) - n;
    let v = r.bits(w - 1)?;
    if v < m {
        return Ok(v);
    }
    let extra = r.bits(1)?;
    Ok((v << 1) - m + extra)
}

fn su(r: &mut BitReader, n: u32) -> Result<i32> {
    let v = r.bits(n)? as i32;
    let sign = 1 << (n - 1);
    Ok(if v & sign != 0 { v - 2 * sign } else { v })
}

fn inverse_recenter(r: i32, v: i32) -> i32 {
    if v > 2 * r {
        v
    } else if v & 1 != 0 {
        r - ((v + 1) >> 1)
    } else {
        r + (v >> 1)
    }
}

fn decode_subexp(r: &mut BitReader, num_syms: i32) -> Result<i32> {
    let (mut i, mut mk, k) = (0, 0, 3);
    loop {
        let b2 = if i != 0 { k + i - 1 } else { k };
        let a = 1 << b2;
        if num_syms <= mk + 3 * a {
            return Ok(ns(r, (num_syms - mk) as u32)? as i32 + mk);
        }
        if r.flag()? {
            i += 1;
            mk += a;
        } else {
            return Ok(r.bits(b2 as u32)? as i32 + mk);
        }
    }
}

fn decode_unsigned_subexp_with_ref(r: &mut BitReader, mx: i32, rf: i32) -> Result<i32> {
    let v = decode_subexp(r, mx)?;
    Ok(if (rf << 1) <= mx { inverse_recenter(rf, v) } else { mx - 1 - inverse_recenter(mx - 1 - rf, v) })
}

fn decode_signed_subexp_with_ref(r: &mut BitReader, low: i32, high: i32, rf: i32) -> Result<i32> {
    Ok(decode_unsigned_subexp_with_ref(r, high - low, rf - low)? + low)
}

impl Parser {
    pub fn new() -> Self {
        Self { seq: None, slots: Default::default(), temporal_id: 0, spatial_id: 0, current_frame_id: 0 }
    }

    pub fn reset(&mut self) {
        self.slots = Default::default();
    }

    fn rel_dist(&self, a: u32, b: u32) -> i32 {
        let s = self.seq.as_ref().unwrap();
        if !s.enable_order_hint {
            return 0;
        }
        let diff = a as i32 - b as i32;
        let m = 1i32 << (s.order_hint_bits - 1);
        (diff & (m - 1)) - (diff & m)
    }

    /// Reads a frame header (the payload of a frame header OBU, or the start of a frame OBU).
    pub fn parse_frame_header(&mut self, payload: &[u8]) -> Result<Header> {
        let seq = self.seq.clone().ok_or(Error::Invalid("AV1 frame before the sequence header"))?;
        let mut r = BitReader::new(payload);
        let id_len = seq.additional_frame_id_length + seq.delta_frame_id_length;
        let mut h = Header {
            show_existing_frame: false,
            frame_to_show: 0,
            frame_type: KEY_FRAME,
            frame_is_intra: true,
            show_frame: true,
            showable_frame: false,
            error_resilient_mode: false,
            disable_cdf_update: false,
            allow_screen_content_tools: false,
            force_integer_mv: false,
            frame_size_override_flag: false,
            order_hint: 0,
            primary_ref_frame: PRIMARY_REF_NONE,
            refresh_frame_flags: 0,
            ref_frame_idx: [0; 7],
            allow_high_precision_mv: false,
            use_ref_frame_mvs: false,
            allow_intrabc: false,
            is_motion_mode_switchable: false,
            interpolation_filter: 0,
            disable_frame_end_update_cdf: true,
            upscaled_width: 0,
            frame_width: 0,
            frame_height: 0,
            render_width: 0,
            render_height: 0,
            use_superres: false,
            superres_denom: 8,
            mi_cols: 0,
            mi_rows: 0,
            tile_cols: 1,
            tile_rows: 1,
            tile_cols_log2: 0,
            tile_rows_log2: 0,
            tile_widths_sb: vec![],
            tile_heights_sb: vec![],
            context_update_tile_id: 0,
            tile_size_bytes: 4,
            base_q_idx: 0,
            delta_q_y_dc: 0,
            delta_q_u_dc: 0,
            delta_q_u_ac: 0,
            delta_q_v_dc: 0,
            delta_q_v_ac: 0,
            using_qmatrix: false,
            qm_y: 0,
            qm_u: 0,
            qm_v: 0,
            seg: Segmentation::default(),
            delta_q_present: false,
            delta_q_res: 0,
            delta_lf_present: false,
            delta_lf_res: 0,
            delta_lf_multi: false,
            coded_lossless: false,
            loop_filter_level: [0; 4],
            sharpness: 0,
            lf_delta_enabled: false,
            lf_delta_update: false,
            ref_deltas: DEFAULT_REF_DELTAS,
            mode_deltas: [0; 2],
            cdef_damping_minus_3: 0,
            cdef_bits: 0,
            cdef_y_pri: [0; 8],
            cdef_y_sec: [0; 8],
            cdef_uv_pri: [0; 8],
            cdef_uv_sec: [0; 8],
            lr_type: [0; 3],
            lr_unit_shift: 0,
            lr_uv_shift: 0,
            uses_lr: false,
            tx_mode: 0,
            reference_select: false,
            skip_mode_present: false,
            allow_warped_motion: false,
            reduced_tx_set: false,
            gm_type: [0; 8],
            gm_params: identity_gm(),
            film_grain: FilmGrain::default(),
            order_hints: [0; 8],
            header_bytes: 0,
        };
        let all_frames = 0xffu8;
        if seq.reduced_still_picture_header {
            h.frame_type = KEY_FRAME;
            h.show_frame = true;
        } else {
            h.show_existing_frame = r.flag()?;
            if h.show_existing_frame {
                h.frame_to_show = r.bits(3)? as u8;
                if seq.decoder_model_info_present && !seq.equal_picture_interval {
                    r.skip(seq.frame_presentation_time_length as usize)?;
                }
                if seq.frame_id_numbers_present {
                    r.skip(id_len as usize)?;
                }
                let slot = &self.slots[h.frame_to_show as usize];
                if !slot.valid {
                    return Err(Error::Invalid("AV1 show_existing_frame of an empty slot"));
                }
                h.frame_type = slot.frame_type;
                h.show_frame = true;
                h.upscaled_width = slot.upscaled_width;
                h.frame_width = slot.frame_width;
                h.frame_height = slot.frame_height;
                if h.frame_type == KEY_FRAME {
                    h.refresh_frame_flags = all_frames;
                }
                if seq.film_grain_params_present {
                    h.film_grain = slot.film_grain.clone();
                }
                h.header_bytes = r.position().div_ceil(8);
                return Ok(h);
            }
            h.frame_type = r.bits(2)? as u8;
            h.frame_is_intra = h.frame_type == INTRA_ONLY_FRAME || h.frame_type == KEY_FRAME;
            h.show_frame = r.flag()?;
            if h.show_frame && seq.decoder_model_info_present && !seq.equal_picture_interval {
                r.skip(seq.frame_presentation_time_length as usize)?;
            }
            h.showable_frame = if h.show_frame { h.frame_type != KEY_FRAME } else { r.flag()? };
            h.error_resilient_mode = if h.frame_type == SWITCH_FRAME || (h.frame_type == KEY_FRAME && h.show_frame) { true } else { r.flag()? };
        }
        h.frame_is_intra = h.frame_type == INTRA_ONLY_FRAME || h.frame_type == KEY_FRAME;
        if h.frame_type == KEY_FRAME && h.show_frame {
            for s in self.slots.iter_mut() {
                s.valid = false;
                s.order_hint = 0;
            }
        }
        h.disable_cdf_update = r.flag()?;
        h.allow_screen_content_tools = if seq.seq_force_screen_content_tools == SELECT { r.flag()? } else { seq.seq_force_screen_content_tools != 0 };
        if h.allow_screen_content_tools {
            h.force_integer_mv = if seq.seq_force_integer_mv == SELECT { r.flag()? } else { seq.seq_force_integer_mv != 0 };
        }
        if seq.frame_id_numbers_present {
            self.current_frame_id = r.bits(id_len)?;
        }
        h.frame_size_override_flag = if h.frame_type == SWITCH_FRAME {
            true
        } else if seq.reduced_still_picture_header {
            false
        } else {
            r.flag()?
        };
        h.order_hint = r.bits(seq.order_hint_bits)?;
        h.primary_ref_frame = if h.frame_is_intra || h.error_resilient_mode { PRIMARY_REF_NONE } else { r.bits(3)? as u8 };
        if seq.decoder_model_info_present && r.flag()? {
            for op in 0..seq.operating_point_idcs.len() {
                if seq.decoder_model_present_for_op[op] {
                    let idc = seq.operating_point_idcs[op];
                    let in_t = (idc >> self.temporal_id) & 1 != 0;
                    let in_s = (idc >> (self.spatial_id + 8)) & 1 != 0;
                    if idc == 0 || (in_t && in_s) {
                        r.skip(seq.buffer_removal_time_length as usize)?;
                    }
                }
            }
        }
        h.refresh_frame_flags = if h.frame_type == SWITCH_FRAME || (h.frame_type == KEY_FRAME && h.show_frame) { all_frames } else { r.bits(8)? as u8 };
        if (!h.frame_is_intra || h.refresh_frame_flags != all_frames) && h.error_resilient_mode && seq.enable_order_hint {
            for i in 0..8 {
                let hint = r.bits(seq.order_hint_bits)?;
                if hint != self.slots[i].order_hint || !self.slots[i].valid {
                    self.slots[i].valid = false;
                    self.slots[i].order_hint = hint;
                }
            }
        }
        if h.frame_type == KEY_FRAME || h.frame_type == INTRA_ONLY_FRAME {
            self.frame_size(&seq, &mut h, &mut r)?;
            self.render_size(&mut h, &mut r)?;
            if h.allow_screen_content_tools && h.upscaled_width == h.frame_width {
                h.allow_intrabc = r.flag()?;
            }
        } else {
            let mut short_signaling = false;
            if seq.enable_order_hint {
                short_signaling = r.flag()?;
                if short_signaling {
                    let last = r.bits(3)? as usize;
                    let gold = r.bits(3)? as usize;
                    h.ref_frame_idx = self.set_frame_refs(&seq, h.order_hint, last, gold);
                }
            }
            for i in 0..7 {
                if !short_signaling {
                    h.ref_frame_idx[i] = r.bits(3)? as u8;
                }
                if seq.frame_id_numbers_present {
                    r.skip(seq.delta_frame_id_length as usize)?;
                }
            }
            for i in 0..7 {
                if !self.slots[h.ref_frame_idx[i] as usize].valid {
                    return Err(Error::Invalid("AV1 frame refers to an empty slot"));
                }
            }
            if h.frame_size_override_flag && !h.error_resilient_mode {
                // frame_size_with_refs
                let mut found = false;
                for i in 0..7 {
                    if r.flag()? {
                        let s = &self.slots[h.ref_frame_idx[i] as usize];
                        h.upscaled_width = s.upscaled_width;
                        h.frame_width = h.upscaled_width;
                        h.frame_height = s.frame_height;
                        h.render_width = s.render_width;
                        h.render_height = s.render_height;
                        found = true;
                        break;
                    }
                }
                if found {
                    self.superres_params(&seq, &mut h, &mut r)?;
                    Self::compute_image_size(&mut h);
                } else {
                    self.frame_size(&seq, &mut h, &mut r)?;
                    self.render_size(&mut h, &mut r)?;
                }
            } else {
                self.frame_size(&seq, &mut h, &mut r)?;
                self.render_size(&mut h, &mut r)?;
            }
            h.allow_high_precision_mv = if h.force_integer_mv { false } else { r.flag()? };
            h.interpolation_filter = if r.flag()? { 4 } else { r.bits(2)? as u8 };
            h.is_motion_mode_switchable = r.flag()?;
            h.use_ref_frame_mvs = if h.error_resilient_mode || !seq.enable_ref_frame_mvs { false } else { r.flag()? };
            for i in 0..7 {
                h.order_hints[i + 1] = self.slots[h.ref_frame_idx[i] as usize].order_hint;
            }
        }
        h.disable_frame_end_update_cdf = if seq.reduced_still_picture_header || h.disable_cdf_update { true } else { r.flag()? };

        // setup_past_independence / load_previous
        let mut prev_gm = identity_gm();
        if h.primary_ref_frame == PRIMARY_REF_NONE {
            h.ref_deltas = DEFAULT_REF_DELTAS;
            h.mode_deltas = [0; 2];
            h.lf_delta_enabled = true;
        } else {
            let s = &self.slots[h.ref_frame_idx[h.primary_ref_frame as usize] as usize];
            prev_gm = s.gm_params;
            h.ref_deltas = s.ref_deltas;
            h.mode_deltas = s.mode_deltas;
            h.seg = s.seg.clone();
        }
        self.tile_info(&seq, &mut h, &mut r)?;
        // quantization_params
        h.base_q_idx = r.bits(8)? as u8;
        let dq = |r: &mut BitReader| -> Result<i8> { Ok(if r.flag()? { su(r, 7)? as i8 } else { 0 }) };
        h.delta_q_y_dc = dq(&mut r)?;
        if seq.num_planes() > 1 {
            let diff_uv = if seq.separate_uv_delta_q { r.flag()? } else { false };
            h.delta_q_u_dc = dq(&mut r)?;
            h.delta_q_u_ac = dq(&mut r)?;
            if diff_uv {
                h.delta_q_v_dc = dq(&mut r)?;
                h.delta_q_v_ac = dq(&mut r)?;
            } else {
                h.delta_q_v_dc = h.delta_q_u_dc;
                h.delta_q_v_ac = h.delta_q_u_ac;
            }
        }
        h.using_qmatrix = r.flag()?;
        if h.using_qmatrix {
            h.qm_y = r.bits(4)? as u8;
            h.qm_u = r.bits(4)? as u8;
            h.qm_v = if seq.separate_uv_delta_q { r.bits(4)? as u8 } else { h.qm_u };
        }
        // segmentation_params
        const BITS: [u32; 8] = [8, 6, 6, 6, 6, 3, 0, 0];
        const SIGNED: [bool; 8] = [true, true, true, true, true, false, false, false];
        const MAX: [i32; 8] = [255, 63, 63, 63, 63, 7, 0, 0];
        h.seg.enabled = r.flag()?;
        h.seg.update_map = false;
        h.seg.temporal_update = false;
        h.seg.update_data = false;
        if h.seg.enabled {
            if h.primary_ref_frame == PRIMARY_REF_NONE {
                h.seg.update_map = true;
                h.seg.update_data = true;
            } else {
                h.seg.update_map = r.flag()?;
                if h.seg.update_map {
                    h.seg.temporal_update = r.flag()?;
                }
                h.seg.update_data = r.flag()?;
            }
            if h.seg.update_data {
                h.seg.feature_enabled = [[false; 8]; 8];
                h.seg.feature_data = [[0; 8]; 8];
                for i in 0..8 {
                    for j in 0..8 {
                        let en = r.flag()?;
                        h.seg.feature_enabled[i][j] = en;
                        if en {
                            let limit = MAX[j];
                            let v = if SIGNED[j] { su(&mut r, 1 + BITS[j])?.clamp(-limit, limit) } else { (r.bits(BITS[j])? as i32).clamp(0, limit) };
                            h.seg.feature_data[i][j] = v as i16;
                        }
                    }
                }
            }
        } else {
            h.seg.feature_enabled = [[false; 8]; 8];
            h.seg.feature_data = [[0; 8]; 8];
        }
        // delta_q_params, delta_lf_params
        if h.base_q_idx > 0 {
            h.delta_q_present = r.flag()?;
        }
        if h.delta_q_present {
            h.delta_q_res = r.bits(2)? as u8;
            if !h.allow_intrabc {
                h.delta_lf_present = r.flag()?;
            }
            if h.delta_lf_present {
                h.delta_lf_res = r.bits(2)? as u8;
                h.delta_lf_multi = r.flag()?;
            }
        }
        // lossless
        let mut all_lossless_seg = true;
        for seg_id in 0..8 {
            let q = self.qidx(&h, seg_id);
            let lossless = q == 0 && h.delta_q_y_dc == 0 && h.delta_q_u_ac == 0 && h.delta_q_u_dc == 0 && h.delta_q_v_ac == 0 && h.delta_q_v_dc == 0;
            all_lossless_seg &= lossless;
        }
        h.coded_lossless = all_lossless_seg;
        let all_lossless = h.coded_lossless && h.frame_width == h.upscaled_width;
        // loop_filter_params
        if h.coded_lossless || h.allow_intrabc {
            h.loop_filter_level[0] = 0;
            h.loop_filter_level[1] = 0;
            h.ref_deltas = DEFAULT_REF_DELTAS;
            h.mode_deltas = [0; 2];
        } else {
            h.loop_filter_level[0] = r.bits(6)? as u8;
            h.loop_filter_level[1] = r.bits(6)? as u8;
            if seq.num_planes() > 1 && (h.loop_filter_level[0] != 0 || h.loop_filter_level[1] != 0) {
                h.loop_filter_level[2] = r.bits(6)? as u8;
                h.loop_filter_level[3] = r.bits(6)? as u8;
            }
            h.sharpness = r.bits(3)? as u8;
            h.lf_delta_enabled = r.flag()?;
            if h.lf_delta_enabled {
                h.lf_delta_update = r.flag()?;
                if h.lf_delta_update {
                    for i in 0..8 {
                        if r.flag()? {
                            h.ref_deltas[i] = su(&mut r, 7)? as i8;
                        }
                    }
                    for i in 0..2 {
                        if r.flag()? {
                            h.mode_deltas[i] = su(&mut r, 7)? as i8;
                        }
                    }
                }
            }
        }
        // cdef_params
        if !(h.coded_lossless || h.allow_intrabc || !seq.enable_cdef) {
            h.cdef_damping_minus_3 = r.bits(2)? as u8;
            h.cdef_bits = r.bits(2)? as u8;
            for i in 0..(1usize << h.cdef_bits) {
                h.cdef_y_pri[i] = r.bits(4)? as u8;
                h.cdef_y_sec[i] = r.bits(2)? as u8;
                if seq.num_planes() > 1 {
                    h.cdef_uv_pri[i] = r.bits(4)? as u8;
                    h.cdef_uv_sec[i] = r.bits(2)? as u8;
                }
            }
        }
        // lr_params
        if !(all_lossless || h.allow_intrabc || !seq.enable_restoration) {
            const REMAP: [u8; 4] = [0, 3, 1, 2];
            let mut chroma_lr = false;
            for i in 0..seq.num_planes() {
                h.lr_type[i] = REMAP[r.bits(2)? as usize];
                if h.lr_type[i] != 0 {
                    h.uses_lr = true;
                    if i > 0 {
                        chroma_lr = true;
                    }
                }
            }
            if h.uses_lr {
                if seq.use_128x128_superblock {
                    h.lr_unit_shift = r.bits(1)? as u8 + 1;
                } else {
                    h.lr_unit_shift = r.bits(1)? as u8;
                    if h.lr_unit_shift != 0 {
                        h.lr_unit_shift += r.bits(1)? as u8;
                    }
                }
                if seq.subsampling_x && seq.subsampling_y && chroma_lr {
                    h.lr_uv_shift = r.bits(1)? as u8;
                }
            }
        }
        // read_tx_mode
        h.tx_mode = if h.coded_lossless { 0 } else if r.flag()? { 2 } else { 1 };
        h.reference_select = if h.frame_is_intra { false } else { r.flag()? };
        // skip_mode_params
        let mut skip_allowed = false;
        if !(h.frame_is_intra || !h.reference_select || !seq.enable_order_hint) {
            let (mut fwd, mut bwd) = (-1i32, -1i32);
            let (mut fwd_hint, mut bwd_hint) = (0u32, 0u32);
            for i in 0..7 {
                let hint = self.slots[h.ref_frame_idx[i] as usize].order_hint;
                if self.rel_dist(hint, h.order_hint) < 0 {
                    if fwd < 0 || self.rel_dist(hint, fwd_hint) > 0 {
                        fwd = i as i32;
                        fwd_hint = hint;
                    }
                } else if self.rel_dist(hint, h.order_hint) > 0 && (bwd < 0 || self.rel_dist(hint, bwd_hint) < 0) {
                    bwd = i as i32;
                    bwd_hint = hint;
                }
            }
            if fwd < 0 {
                skip_allowed = false;
            } else if bwd >= 0 {
                skip_allowed = true;
            } else {
                let (mut second, mut second_hint) = (-1i32, 0u32);
                for i in 0..7 {
                    let hint = self.slots[h.ref_frame_idx[i] as usize].order_hint;
                    if self.rel_dist(hint, fwd_hint) < 0 && (second < 0 || self.rel_dist(hint, second_hint) > 0) {
                        second = i as i32;
                        second_hint = hint;
                    }
                }
                skip_allowed = second >= 0;
            }
        }
        h.skip_mode_present = if skip_allowed { r.flag()? } else { false };
        h.allow_warped_motion = if h.frame_is_intra || h.error_resilient_mode || !seq.enable_warped_motion { false } else { r.flag()? };
        h.reduced_tx_set = r.flag()?;
        // global_motion_params
        if !h.frame_is_intra {
            for rf in 1..8 {
                let ty = if r.flag()? {
                    if r.flag()? {
                        2
                    } else if r.flag()? {
                        1
                    } else {
                        3
                    }
                } else {
                    0
                };
                h.gm_type[rf] = ty;
                if ty >= 2 {
                    self.read_global_param(&mut r, &mut h, &prev_gm, rf, 2)?;
                    self.read_global_param(&mut r, &mut h, &prev_gm, rf, 3)?;
                    if ty == 3 {
                        self.read_global_param(&mut r, &mut h, &prev_gm, rf, 4)?;
                        self.read_global_param(&mut r, &mut h, &prev_gm, rf, 5)?;
                    } else {
                        h.gm_params[rf][4] = -h.gm_params[rf][3];
                        h.gm_params[rf][5] = h.gm_params[rf][2];
                    }
                }
                if ty >= 1 {
                    self.read_global_param(&mut r, &mut h, &prev_gm, rf, 0)?;
                    self.read_global_param(&mut r, &mut h, &prev_gm, rf, 1)?;
                }
            }
        }
        // film_grain_params
        self.film_grain(&seq, &mut h, &mut r)?;
        h.header_bytes = r.position().div_ceil(8);
        Ok(h)
    }

    fn qidx(&self, h: &Header, seg_id: usize) -> i32 {
        if h.seg.enabled && h.seg.feature_enabled[seg_id][0] {
            (h.base_q_idx as i32 + h.seg.feature_data[seg_id][0] as i32).clamp(0, 255)
        } else {
            h.base_q_idx as i32
        }
    }

    fn superres_params(&self, seq: &Sequence, h: &mut Header, r: &mut BitReader) -> Result<()> {
        h.use_superres = if seq.enable_superres { r.flag()? } else { false };
        h.superres_denom = if h.use_superres { r.bits(3)? + 9 } else { 8 };
        h.upscaled_width = h.frame_width;
        h.frame_width = (h.upscaled_width * 8 + h.superres_denom / 2) / h.superres_denom;
        Ok(())
    }

    fn compute_image_size(h: &mut Header) {
        h.mi_cols = 2 * h.frame_width.div_ceil(8);
        h.mi_rows = 2 * h.frame_height.div_ceil(8);
    }

    fn frame_size(&self, seq: &Sequence, h: &mut Header, r: &mut BitReader) -> Result<()> {
        if h.frame_size_override_flag {
            h.frame_width = r.bits(seq.frame_width_bits)? + 1;
            h.frame_height = r.bits(seq.frame_height_bits)? + 1;
        } else {
            h.frame_width = seq.max_frame_width;
            h.frame_height = seq.max_frame_height;
        }
        self.superres_params(seq, h, r)?;
        Self::compute_image_size(h);
        Ok(())
    }

    fn render_size(&self, h: &mut Header, r: &mut BitReader) -> Result<()> {
        if r.flag()? {
            h.render_width = r.bits(16)? + 1;
            h.render_height = r.bits(16)? + 1;
        } else {
            h.render_width = h.upscaled_width;
            h.render_height = h.frame_height;
        }
        Ok(())
    }

    /// 7.8, set_frame_refs: the reference indices when only `last` and `gold` are sent.
    fn set_frame_refs(&self, seq: &Sequence, order_hint: u32, last: usize, gold: usize) -> [u8; 7] {
        let mut idx = [-1i32; 7];
        idx[0] = last as i32;
        idx[3] = gold as i32;
        let mut used = [false; 8];
        used[last] = true;
        used[gold] = true;
        let cur = 1i32 << (seq.order_hint_bits - 1);
        let mut shifted = [0i32; 8];
        for i in 0..8 {
            shifted[i] = cur + self.rel_dist(self.slots[i].order_hint, order_hint);
        }
        let last_hint = shifted[last];
        let gold_hint = shifted[gold];
        let _ = (last_hint, gold_hint);
        // find_latest_backward / earliest_backward / latest_forward
        let find_latest_backward = |used: &[bool; 8]| -> i32 {
            let (mut r, mut latest) = (-1i32, 0i32);
            for i in 0..8 {
                let hint = shifted[i];
                if !used[i] && hint >= cur && (r < 0 || hint >= latest) {
                    r = i as i32;
                    latest = hint;
                }
            }
            r
        };
        let find_earliest_backward = |used: &[bool; 8]| -> i32 {
            let (mut r, mut earliest) = (-1i32, 0i32);
            for i in 0..8 {
                let hint = shifted[i];
                if !used[i] && hint >= cur && (r < 0 || hint < earliest) {
                    r = i as i32;
                    earliest = hint;
                }
            }
            r
        };
        let find_latest_forward = |used: &[bool; 8]| -> i32 {
            let (mut r, mut latest) = (-1i32, 0i32);
            for i in 0..8 {
                let hint = shifted[i];
                if !used[i] && hint < cur && (r < 0 || hint >= latest) {
                    r = i as i32;
                    latest = hint;
                }
            }
            r
        };
        // ALTREF (index 6), BWDREF (4), ALTREF2 (5)
        let r = find_latest_backward(&used);
        if r >= 0 {
            idx[6] = r;
            used[r as usize] = true;
        }
        let r = find_earliest_backward(&used);
        if r >= 0 {
            idx[4] = r;
            used[r as usize] = true;
        }
        let r = find_earliest_backward(&used);
        if r >= 0 {
            idx[5] = r;
            used[r as usize] = true;
        }
        // the rest, in this order: LAST2, LAST3, BWDREF, ALTREF2, ALTREF
        for &refi in &[1usize, 2, 4, 5, 6] {
            if idx[refi] < 0 {
                let r = find_latest_forward(&used);
                if r >= 0 {
                    idx[refi] = r;
                    used[r as usize] = true;
                }
            }
        }
        // anything left gets the reference with the earliest hint
        let (mut r, mut earliest) = (-1i32, 0i32);
        for i in 0..8 {
            let hint = shifted[i];
            if r < 0 || hint < earliest {
                r = i as i32;
                earliest = hint;
            }
        }
        let mut out = [0u8; 7];
        for i in 0..7 {
            out[i] = if idx[i] < 0 { r as u8 } else { idx[i] as u8 };
        }
        out
    }

    fn tile_info(&self, seq: &Sequence, h: &mut Header, r: &mut BitReader) -> Result<()> {
        let sb_shift = if seq.use_128x128_superblock { 5 } else { 4 };
        let sb_cols = (h.mi_cols + (1 << sb_shift) - 1) >> sb_shift;
        let sb_rows = (h.mi_rows + (1 << sb_shift) - 1) >> sb_shift;
        let sb_size = sb_shift + 2;
        let max_tile_width_sb = 4096 >> sb_size;
        let mut max_tile_area_sb = (4096 * 2304) >> (2 * sb_size);
        let min_log2_tile_cols = tile_log2(max_tile_width_sb, sb_cols);
        let max_log2_tile_cols = tile_log2(1, sb_cols.min(64));
        let max_log2_tile_rows = tile_log2(1, sb_rows.min(64));
        let min_log2_tiles = min_log2_tile_cols.max(tile_log2(max_tile_area_sb, sb_rows * sb_cols));
        h.tile_widths_sb.clear();
        h.tile_heights_sb.clear();
        if r.flag()? {
            h.tile_cols_log2 = min_log2_tile_cols;
            while h.tile_cols_log2 < max_log2_tile_cols {
                if r.flag()? {
                    h.tile_cols_log2 += 1;
                } else {
                    break;
                }
            }
            let tw = (sb_cols + (1 << h.tile_cols_log2) - 1) >> h.tile_cols_log2;
            let mut start = 0;
            while start < sb_cols {
                h.tile_widths_sb.push(tw.min(sb_cols - start));
                start += tw;
            }
            let min_log2_tile_rows = min_log2_tiles.saturating_sub(h.tile_cols_log2);
            h.tile_rows_log2 = min_log2_tile_rows;
            while h.tile_rows_log2 < max_log2_tile_rows {
                if r.flag()? {
                    h.tile_rows_log2 += 1;
                } else {
                    break;
                }
            }
            let th = (sb_rows + (1 << h.tile_rows_log2) - 1) >> h.tile_rows_log2;
            let mut start = 0;
            while start < sb_rows {
                h.tile_heights_sb.push(th.min(sb_rows - start));
                start += th;
            }
        } else {
            let mut widest = 0;
            let mut start = 0;
            while start < sb_cols {
                let max_w = (sb_cols - start).min(max_tile_width_sb);
                let size = ns(r, max_w)? + 1;
                h.tile_widths_sb.push(size);
                widest = widest.max(size);
                start += size;
            }
            h.tile_cols_log2 = tile_log2(1, h.tile_widths_sb.len() as u32);
            max_tile_area_sb = if min_log2_tiles > 0 { (sb_rows * sb_cols) >> (min_log2_tiles + 1) } else { sb_rows * sb_cols };
            let max_tile_height_sb = (max_tile_area_sb / widest).max(1);
            let mut start = 0;
            while start < sb_rows {
                let max_h = (sb_rows - start).min(max_tile_height_sb);
                let size = ns(r, max_h)? + 1;
                h.tile_heights_sb.push(size);
                start += size;
            }
            h.tile_rows_log2 = tile_log2(1, h.tile_heights_sb.len() as u32);
        }
        h.tile_cols = h.tile_widths_sb.len() as u32;
        h.tile_rows = h.tile_heights_sb.len() as u32;
        if h.tile_cols > 64 || h.tile_rows > 64 {
            return Err(Error::Invalid("AV1 tile count"));
        }
        if h.tile_cols_log2 > 0 || h.tile_rows_log2 > 0 {
            h.context_update_tile_id = r.bits(h.tile_rows_log2 + h.tile_cols_log2)?;
            h.tile_size_bytes = r.bits(2)? + 1;
        }
        Ok(())
    }

    fn read_global_param(&self, r: &mut BitReader, h: &mut Header, prev: &[[i32; 6]; 8], rf: usize, idx: usize) -> Result<()> {
        let mut abs_bits = 12;
        let mut prec_bits = 15;
        if idx < 2 {
            if h.gm_type[rf] == 1 {
                abs_bits = 9 - (!h.allow_high_precision_mv) as i32;
                prec_bits = 3 - (!h.allow_high_precision_mv) as i32;
            } else {
                abs_bits = 12;
                prec_bits = 6;
            }
        }
        let prec_diff = WARP_PREC - prec_bits;
        let round = if idx % 3 == 2 { 1 << WARP_PREC } else { 0 };
        let sub = if idx % 3 == 2 { 1 << prec_bits } else { 0 };
        let mx = 1 << abs_bits;
        let rf_val = (prev[rf][idx] >> prec_diff) - sub;
        h.gm_params[rf][idx] = (decode_signed_subexp_with_ref(r, -mx, mx + 1, rf_val)? << prec_diff) + round;
        Ok(())
    }

    fn film_grain(&self, seq: &Sequence, h: &mut Header, r: &mut BitReader) -> Result<()> {
        h.film_grain = FilmGrain::default();
        if !seq.film_grain_params_present || (!h.show_frame && !h.showable_frame) {
            return Ok(());
        }
        let mut g = FilmGrain { apply_grain: r.flag()?, ..Default::default() };
        if !g.apply_grain {
            return Ok(());
        }
        g.grain_seed = r.bits(16)? as u16;
        let update = if h.frame_type == INTER_FRAME { r.flag()? } else { true };
        if !update {
            let idx = r.bits(3)? as usize;
            let seed = g.grain_seed;
            g = self.slots[idx].film_grain.clone();
            g.grain_seed = seed;
            g.apply_grain = true;
            h.film_grain = g;
            return Ok(());
        }
        let n = r.bits(4)?;
        for _ in 0..n {
            g.scaling_points_y.push((r.bits(8)? as u8, r.bits(8)? as u8));
        }
        g.chroma_scaling_from_luma = if seq.mono_chrome { false } else { r.flag()? };
        if !(seq.mono_chrome || g.chroma_scaling_from_luma || (seq.subsampling_x && seq.subsampling_y && n == 0)) {
            let n = r.bits(4)?;
            for _ in 0..n {
                g.scaling_points_cb.push((r.bits(8)? as u8, r.bits(8)? as u8));
            }
            let n = r.bits(4)?;
            for _ in 0..n {
                g.scaling_points_cr.push((r.bits(8)? as u8, r.bits(8)? as u8));
            }
        }
        g.grain_scaling_minus_8 = r.bits(2)? as u8;
        g.ar_coeff_lag = r.bits(2)? as u8;
        let num_pos_luma = 2 * g.ar_coeff_lag as usize * (g.ar_coeff_lag as usize + 1);
        let num_pos_chroma;
        if !g.scaling_points_y.is_empty() {
            num_pos_chroma = num_pos_luma + 1;
            for _ in 0..num_pos_luma {
                g.ar_coeffs_y_plus_128.push(r.bits(8)? as u8);
            }
        } else {
            num_pos_chroma = num_pos_luma;
        }
        if g.chroma_scaling_from_luma || !g.scaling_points_cb.is_empty() {
            for _ in 0..num_pos_chroma {
                g.ar_coeffs_cb_plus_128.push(r.bits(8)? as u8);
            }
        }
        if g.chroma_scaling_from_luma || !g.scaling_points_cr.is_empty() {
            for _ in 0..num_pos_chroma {
                g.ar_coeffs_cr_plus_128.push(r.bits(8)? as u8);
            }
        }
        g.ar_coeff_shift_minus_6 = r.bits(2)? as u8;
        g.grain_scale_shift = r.bits(2)? as u8;
        if !g.scaling_points_cb.is_empty() {
            g.cb_mult = r.bits(8)? as u8;
            g.cb_luma_mult = r.bits(8)? as u8;
            g.cb_offset = r.bits(9)? as u16;
        }
        if !g.scaling_points_cr.is_empty() {
            g.cr_mult = r.bits(8)? as u8;
            g.cr_luma_mult = r.bits(8)? as u8;
            g.cr_offset = r.bits(9)? as u16;
        }
        g.overlap_flag = r.flag()?;
        g.clip_to_restricted_range = r.flag()?;
        h.film_grain = g;
        Ok(())
    }

    /// Stores what a decoded frame leaves in the slots it refreshes (7.20), or for a shown key
    /// frame found with `show_existing_frame`, copies that slot into all slots (7.21).
    pub fn finish(&mut self, h: &Header) {
        let Some(seq) = self.seq.as_ref() else { return };
        if h.show_existing_frame {
            if h.frame_type == KEY_FRAME {
                let s = self.slots[h.frame_to_show as usize].clone();
                for slot in self.slots.iter_mut() {
                    *slot = s.clone();
                }
            }
            return;
        }
        let st = SlotState {
            valid: true,
            frame_id: self.current_frame_id,
            upscaled_width: h.upscaled_width,
            frame_width: h.frame_width,
            frame_height: h.frame_height,
            render_width: h.render_width,
            render_height: h.render_height,
            frame_type: h.frame_type,
            order_hint: h.order_hint,
            saved_order_hints: h.order_hints,
            gm_params: h.gm_params,
            ref_deltas: h.ref_deltas,
            mode_deltas: h.mode_deltas,
            seg: h.seg.clone(),
            film_grain: h.film_grain.clone(),
            showable: h.showable_frame,
            bit_depth: seq.bit_depth,
        };
        for i in 0..8 {
            if h.refresh_frame_flags >> i & 1 != 0 {
                self.slots[i] = st.clone();
            }
        }
    }
}

/// Where the tile data of a tile group starts, and the tiles in it.
#[derive(Debug, Clone, Copy)]
pub struct TileEntry {
    /// Offset of the tile's data from the start of the tile group's tile data.
    pub offset: usize,
    pub size: usize,
    pub row: u16,
    pub col: u16,
}

/// Reads a tile group OBU body (`tile_group_obu`, 5.11.1), after the frame header if it is part
/// of a frame OBU. Returns `(tg_start, tg_end, tile_data_offset, tiles)`: the tile data begins at
/// `tile_data_offset` in `body`.
pub fn parse_tile_group(h: &Header, body: &[u8]) -> Result<(u32, u32, usize, Vec<TileEntry>)> {
    let num_tiles = h.tile_cols * h.tile_rows;
    let mut r = BitReader::new(body);
    let (mut start, mut end) = (0, num_tiles - 1);
    if num_tiles > 1 && r.flag()? {
        let bits = h.tile_cols_log2 + h.tile_rows_log2;
        start = r.bits(bits)?;
        end = r.bits(bits)?;
    }
    if end >= num_tiles || start > end {
        return Err(Error::Invalid("AV1 tile group range"));
    }
    let data_at = r.position().div_ceil(8);
    let mut pos = data_at;
    let mut tiles = Vec::new();
    for t in start..=end {
        let size;
        if t == end {
            size = body.len().checked_sub(pos).ok_or(Error::Invalid("AV1 tile group cut short"))?;
        } else {
            let n = h.tile_size_bytes as usize;
            if pos + n > body.len() {
                return Err(Error::Invalid("AV1 tile size cut short"));
            }
            let mut v = 0usize;
            for k in 0..n {
                v |= (body[pos + k] as usize) << (8 * k);
            }
            size = v + 1;
            pos += n;
        }
        if pos + size > body.len() {
            return Err(Error::Invalid("AV1 tile runs past the tile group"));
        }
        tiles.push(TileEntry { offset: pos - data_at, size, row: (t / h.tile_cols) as u16, col: (t % h.tile_cols) as u16 });
        pos += size;
    }
    Ok((start, end, data_at, tiles))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn obu_split_with_sizes() {
        // temporal delimiter (type 2, has_size, size 0), padding OBU of 2 bytes
        let d = [0x12, 0x00, 0x7a, 0x02, 0xaa, 0xbb];
        let o = split_obus(&d).unwrap();
        assert_eq!(o.len(), 2);
        assert_eq!(o[0].kind, OBU_TEMPORAL_DELIMITER);
        assert_eq!(o[1].kind, 15);
        assert_eq!(o[1].payload, &[0xaa, 0xbb]);
    }

    #[test]
    fn ns_reads_non_symmetric() {
        // n = 5: w = 3, m = 3; values 0..2 take 2 bits, 3 and 4 take 3 bits
        let d = [0b0111_0000];
        let mut r = BitReader::new(&d);
        assert_eq!(ns(&mut r, 5).unwrap(), 1);
        assert_eq!(ns(&mut r, 5).unwrap(), 3);
    }

    #[test]
    fn recenter() {
        assert_eq!(inverse_recenter(5, 11), 11);
        assert_eq!(inverse_recenter(5, 3), 3);
        assert_eq!(inverse_recenter(5, 4), 7);
    }
}
