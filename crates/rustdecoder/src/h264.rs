//! H.264 parameter sets and slice headers (ITU-T H.264, 7.3.2.1, 7.3.2.2, 7.3.3), as far as the
//! GPU decoder and the picture management need them. The slice data itself is read by the GPU.

use crate::bits::BitReader;
use crate::{Error, Result};

pub mod nal_type {
    pub const SLICE: u8 = 1;
    pub const DPA: u8 = 2;
    pub const DPC: u8 = 4;
    pub const IDR: u8 = 5;
    pub const SPS: u8 = 7;
    pub const PPS: u8 = 8;
    pub const END_OF_SEQUENCE: u8 = 10;
}

pub fn nal_unit_type(unit: &[u8]) -> Option<u8> {
    unit.first().map(|b| b & 0x1f)
}

pub fn nal_ref_idc(unit: &[u8]) -> u8 {
    unit.first().map_or(0, |b| (b >> 5) & 3)
}

/// One scaling list as sent: not present, "use the default list", or the 16 / 64 values in
/// coded (zig-zag) order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListSpec {
    Absent,
    Default,
    Explicit(Vec<u8>),
}

/// The scaling lists of an SPS or PPS as sent (up to 12 lists: six 4x4, then the 8x8 ones).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScalingSpec {
    pub lists: Vec<ListSpec>,
}

/// Scaling lists in effect, in coded order: six 4x4 (Intra Y, Cb, Cr, Inter Y, Cb, Cr) and the
/// two 8x8 luma ones (Intra, Inter), which is all 4:2:0 uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Matrices {
    pub l4: [[u8; 16]; 6],
    pub l8: [[u8; 64]; 2],
}

const DEFAULT_4X4_INTRA: [u8; 16] = [6, 13, 13, 20, 20, 20, 28, 28, 28, 28, 32, 32, 32, 37, 37, 42];
const DEFAULT_4X4_INTER: [u8; 16] = [10, 14, 14, 20, 20, 20, 24, 24, 24, 24, 27, 27, 27, 30, 30, 34];
const DEFAULT_8X8_INTRA: [u8; 64] = [
    6, 10, 10, 13, 11, 13, 16, 16, 16, 16, 18, 18, 18, 18, 18, 23, 23, 23, 23, 23, 23, 25, 25, 25, 25, 25, 25, 25, 27, 27, 27, 27, 27, 27, 27, 27, 29, 29, 29, 29, 29, 29, 29, 31, 31, 31, 31, 31,
    31, 33, 33, 33, 33, 33, 36, 36, 36, 36, 38, 38, 38, 40, 40, 42,
];
const DEFAULT_8X8_INTER: [u8; 64] = [
    9, 13, 13, 15, 13, 15, 17, 17, 17, 17, 19, 19, 19, 19, 19, 21, 21, 21, 21, 21, 21, 22, 22, 22, 22, 22, 22, 22, 24, 24, 24, 24, 24, 24, 24, 24, 25, 25, 25, 25, 25, 25, 25, 27, 27, 27, 27, 27,
    27, 28, 28, 28, 28, 28, 30, 30, 30, 30, 32, 32, 32, 33, 33, 35,
];

impl Matrices {
    pub fn flat() -> Self {
        Self { l4: [[16; 16]; 6], l8: [[16; 64]; 2] }
    }

    pub fn defaults() -> Self {
        Self {
            l4: [DEFAULT_4X4_INTRA, DEFAULT_4X4_INTRA, DEFAULT_4X4_INTRA, DEFAULT_4X4_INTER, DEFAULT_4X4_INTER, DEFAULT_4X4_INTER],
            l8: [DEFAULT_8X8_INTRA, DEFAULT_8X8_INTER],
        }
    }
}

impl ScalingSpec {
    /// Table 7-2: lists that were not sent fall back on the previous list, or for the first of
    /// each kind on `fallback` (the default lists for an SPS, the SPS's lists for a PPS).
    fn resolve(&self, fallback: &Matrices) -> Matrices {
        let d = Matrices::defaults();
        let mut out = Matrices::flat();
        for i in 0..6 {
            out.l4[i] = match self.lists.get(i) {
                Some(ListSpec::Explicit(v)) => v.clone().try_into().unwrap_or([16; 16]),
                Some(ListSpec::Default) => d.l4[i],
                _ if i == 0 || i == 3 => fallback.l4[i],
                _ => out.l4[i - 1],
            };
        }
        for i in 0..2 {
            out.l8[i] = match self.lists.get(6 + i) {
                Some(ListSpec::Explicit(v)) => v.clone().try_into().unwrap_or([16; 64]),
                Some(ListSpec::Default) => d.l8[i],
                _ => fallback.l8[i],
            };
        }
        out
    }
}

/// The scaling lists a picture using `sps` and `pps` is decoded with.
pub fn matrices(sps: &Sps, pps: &Pps) -> Matrices {
    let sps_m = match &sps.scaling {
        Some(s) => s.resolve(&Matrices::defaults()),
        None => Matrices::flat(),
    };
    match &pps.scaling {
        Some(s) => {
            let defaults = Matrices::defaults();
            s.resolve(if sps.scaling.is_some() { &sps_m } else { &defaults })
        }
        None => sps_m,
    }
}

fn scaling_spec(b: &mut BitReader, count: usize) -> Result<ScalingSpec> {
    let mut lists = Vec::with_capacity(count);
    for i in 0..count {
        if !b.flag()? {
            lists.push(ListSpec::Absent);
            continue;
        }
        let size = if i < 6 { 16 } else { 64 };
        let (mut last, mut next) = (8i32, 8i32);
        let mut v = Vec::with_capacity(size);
        let mut use_default = false;
        for j in 0..size {
            if next != 0 {
                let delta = b.se()?;
                next = (last + delta + 256).rem_euclid(256);
                use_default = j == 0 && next == 0;
            }
            let cur = if next == 0 { last } else { next };
            v.push(cur as u8);
            last = cur;
        }
        lists.push(if use_default { ListSpec::Default } else { ListSpec::Explicit(v) });
    }
    Ok(ScalingSpec { lists })
}

#[derive(Debug, Clone, Default)]
pub struct Sps {
    pub profile_idc: u8,
    /// constraint_set0..5 flags, set0 in the top bit.
    pub constraint_flags: u8,
    pub level_idc: u8,
    pub id: u8,
    pub chroma_format_idc: u8,
    pub separate_colour_plane: bool,
    pub bit_depth_luma: u8,
    pub bit_depth_chroma: u8,
    pub transform_bypass: bool,
    pub scaling: Option<ScalingSpec>,
    pub log2_max_frame_num: u8,
    pub poc_type: u8,
    pub log2_max_poc_lsb: u8,
    pub delta_pic_order_always_zero: bool,
    pub offset_for_non_ref_pic: i32,
    pub offset_for_top_to_bottom_field: i32,
    pub offset_for_ref_frame: Vec<i32>,
    pub max_num_ref_frames: u8,
    pub gaps_in_frame_num_allowed: bool,
    pub width_mbs: u32,
    /// Height of a frame in macroblocks.
    pub height_mbs: u32,
    pub frame_mbs_only: bool,
    pub mbaff: bool,
    pub direct_8x8_inference: bool,
    /// (left, right, top, bottom) crop in luma samples.
    pub crop: (u32, u32, u32, u32),
    pub max_num_reorder_frames: Option<u8>,
    pub max_dec_frame_buffering: Option<u8>,
    /// VUI sample aspect ratio, (0, 0) when the stream does not say.
    pub sar: (u16, u16),
}

impl Sps {
    /// Coded size in luma samples.
    pub fn coded_size(&self) -> (u32, u32) {
        (self.width_mbs * 16, self.height_mbs * 16)
    }

    /// Visible size, after cropping.
    pub fn display_size(&self) -> (u32, u32) {
        let (w, h) = self.coded_size();
        (w - self.crop.0 - self.crop.1, h - self.crop.2 - self.crop.3)
    }

    /// Frames the level allows in the decoded picture buffer (Table A-1).
    pub fn level_dpb_frames(&self) -> usize {
        let max_dpb_mbs: u32 = match self.level_idc {
            9 | 10 => 396,
            11 if self.constraint_flags & 0x10 != 0 && matches!(self.profile_idc, 66 | 77 | 88) => 396,
            11 => 900,
            12 | 13 | 20 => 2376,
            21 => 4752,
            22 | 30 => 8100,
            31 => 18000,
            32 => 20480,
            40 | 41 => 32768,
            42 => 34816,
            50 => 110400,
            _ => 184320,
        };
        ((max_dpb_mbs / (self.width_mbs * self.height_mbs).max(1)) as usize).clamp(1, 16)
    }

    /// Frames the picture buffer holds: the stream's own limit if it states one, else the level's.
    pub fn dpb_size(&self) -> usize {
        let base = self.max_dec_frame_buffering.map_or_else(|| self.level_dpb_frames(), |n| n as usize);
        base.max(self.max_num_ref_frames as usize).clamp(1, 16)
    }
}

/// `aspect_ratio_idc` 0..16 (Table E-1): sample aspect ratio, (0, 0) unspecified.
const SAR: [(u16, u16); 17] = [(0, 0), (1, 1), (12, 11), (10, 11), (16, 11), (40, 33), (24, 11), (20, 11), (32, 11), (80, 33), (18, 11), (15, 11), (64, 33), (160, 99), (4, 3), (3, 2), (2, 1)];

fn hrd(b: &mut BitReader) -> Result<()> {
    let cpb_cnt = b.ue()? + 1;
    b.skip(8)?;
    for _ in 0..cpb_cnt {
        b.ue()?;
        b.ue()?;
        b.skip(1)?;
    }
    b.skip(20)
}

/// VUI up to the bitstream restriction: (max_num_reorder_frames, max_dec_frame_buffering).
fn vui(b: &mut BitReader, sar: &mut (u16, u16)) -> Result<Option<(u8, u8)>> {
    if b.flag()? {
        let idc = b.bits(8)? as usize;
        if idc == 255 {
            *sar = (b.bits(16)? as u16, b.bits(16)? as u16);
        } else if let Some(&s) = SAR.get(idc) {
            *sar = s;
        }
    }
    if b.flag()? {
        b.skip(1)?;
    }
    if b.flag()? {
        b.skip(4)?;
        if b.flag()? {
            b.skip(24)?;
        }
    }
    if b.flag()? {
        b.ue()?;
        b.ue()?;
    }
    if b.flag()? {
        b.skip(32)?;
        b.skip(32)?;
        b.skip(1)?;
    }
    let nal_hrd = b.flag()?;
    if nal_hrd {
        hrd(b)?;
    }
    let vcl_hrd = b.flag()?;
    if vcl_hrd {
        hrd(b)?;
    }
    if nal_hrd || vcl_hrd {
        b.skip(1)?;
    }
    b.skip(1)?; // pic_struct_present_flag
    if !b.flag()? {
        return Ok(None);
    }
    b.skip(1)?;
    for _ in 0..4 {
        b.ue()?;
    }
    let reorder = b.ue()?;
    let dpb = b.ue()?;
    Ok(Some((reorder.min(255) as u8, dpb.min(255) as u8)))
}

/// The SPS RBSP after its NAL header byte (emulation prevention removed).
pub fn sps(b: &mut BitReader) -> Result<Sps> {
    let mut s = Sps { profile_idc: b.bits(8)? as u8, constraint_flags: b.bits(8)? as u8, level_idc: b.bits(8)? as u8, id: b.ue()?.min(255) as u8, chroma_format_idc: 1, bit_depth_luma: 8, bit_depth_chroma: 8, ..Default::default() };
    if s.id > 31 {
        return Err(Error::Invalid("SPS id out of range"));
    }
    if matches!(s.profile_idc, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        s.chroma_format_idc = b.ue()?.min(3) as u8;
        if s.chroma_format_idc == 3 {
            s.separate_colour_plane = b.flag()?;
        }
        s.bit_depth_luma = 8 + b.ue()?.min(6) as u8;
        s.bit_depth_chroma = 8 + b.ue()?.min(6) as u8;
        s.transform_bypass = b.flag()?;
        if b.flag()? {
            s.scaling = Some(scaling_spec(b, if s.chroma_format_idc != 3 { 8 } else { 12 })?);
        }
    }
    s.log2_max_frame_num = 4 + b.ue()?.min(12) as u8;
    s.poc_type = b.ue()?.min(3) as u8;
    match s.poc_type {
        0 => s.log2_max_poc_lsb = 4 + b.ue()?.min(12) as u8,
        1 => {
            s.delta_pic_order_always_zero = b.flag()?;
            s.offset_for_non_ref_pic = b.se()?;
            s.offset_for_top_to_bottom_field = b.se()?;
            let n = b.ue()?;
            if n > 255 {
                return Err(Error::Invalid("num_ref_frames_in_pic_order_cnt_cycle"));
            }
            for _ in 0..n {
                s.offset_for_ref_frame.push(b.se()?);
            }
        }
        2 => {}
        _ => return Err(Error::Invalid("pic_order_cnt_type")),
    }
    s.max_num_ref_frames = b.ue()?.min(255) as u8;
    s.gaps_in_frame_num_allowed = b.flag()?;
    s.width_mbs = b.ue()? + 1;
    let map_units = b.ue()? + 1;
    s.frame_mbs_only = b.flag()?;
    s.height_mbs = map_units * if s.frame_mbs_only { 1 } else { 2 };
    if !s.frame_mbs_only {
        s.mbaff = b.flag()?;
    }
    if s.width_mbs > 1024 || s.height_mbs > 1024 {
        return Err(Error::Invalid("picture size"));
    }
    s.direct_8x8_inference = b.flag()?;
    if b.flag()? {
        let (l, r, t, bo) = (b.ue()?, b.ue()?, b.ue()?, b.ue()?);
        let (ux, uy) = if s.chroma_format_idc == 0 { (1, 2 - s.frame_mbs_only as u32) } else { (2, 2 * (2 - s.frame_mbs_only as u32)) };
        s.crop = (l * ux, r * ux, t * uy, bo * uy);
        let (w, h) = s.coded_size();
        if s.crop.0 + s.crop.1 >= w || s.crop.2 + s.crop.3 >= h {
            return Err(Error::Invalid("cropping rectangle"));
        }
    }
    if b.flag()? {
        // a damaged VUI must not lose the stream
        if let Ok(Some((reorder, dpb))) = vui(b, &mut s.sar) {
            s.max_num_reorder_frames = Some(reorder);
            s.max_dec_frame_buffering = Some(dpb);
        }
    }
    Ok(s)
}

#[derive(Debug, Clone, Default)]
pub struct Pps {
    pub id: u8,
    pub sps_id: u8,
    pub entropy_coding_mode: bool,
    pub bottom_field_pic_order_in_frame_present: bool,
    pub num_ref_idx_l0_default: u8,
    pub num_ref_idx_l1_default: u8,
    pub weighted_pred: bool,
    pub weighted_bipred_idc: u8,
    pub pic_init_qp_minus26: i8,
    pub pic_init_qs_minus26: i8,
    pub chroma_qp_index_offset: i8,
    pub second_chroma_qp_index_offset: i8,
    pub deblocking_filter_control_present: bool,
    pub constrained_intra_pred: bool,
    pub redundant_pic_cnt_present: bool,
    pub transform_8x8_mode: bool,
    pub scaling: Option<ScalingSpec>,
}

/// The PPS RBSP after its NAL header byte. `chroma_format_idc_of` gives the chroma format of
/// the SPS it names (the number of scaling lists depends on it).
pub fn pps(b: &mut BitReader, chroma_format_idc_of: impl Fn(u8) -> u8) -> Result<Pps> {
    let id = b.ue()?;
    let sps_id = b.ue()?;
    if id > 255 || sps_id > 31 {
        return Err(Error::Invalid("PPS id out of range"));
    }
    let mut p = Pps { id: id as u8, sps_id: sps_id as u8, ..Default::default() };
    p.entropy_coding_mode = b.flag()?;
    p.bottom_field_pic_order_in_frame_present = b.flag()?;
    if b.ue()? > 0 {
        return Err(Error::Unsupported("H.264 slice groups (FMO)"));
    }
    p.num_ref_idx_l0_default = (b.ue()? + 1).min(32) as u8;
    p.num_ref_idx_l1_default = (b.ue()? + 1).min(32) as u8;
    p.weighted_pred = b.flag()?;
    p.weighted_bipred_idc = b.bits(2)? as u8;
    p.pic_init_qp_minus26 = b.se()? as i8;
    p.pic_init_qs_minus26 = b.se()? as i8;
    p.chroma_qp_index_offset = b.se()? as i8;
    p.deblocking_filter_control_present = b.flag()?;
    p.constrained_intra_pred = b.flag()?;
    p.redundant_pic_cnt_present = b.flag()?;
    p.second_chroma_qp_index_offset = p.chroma_qp_index_offset;
    if b.more_rbsp_data() {
        p.transform_8x8_mode = b.flag()?;
        if b.flag()? {
            let n = 6 + if chroma_format_idc_of(p.sps_id) != 3 { 2 } else { 6 } * p.transform_8x8_mode as usize;
            p.scaling = Some(scaling_spec(b, n)?);
        }
        p.second_chroma_qp_index_offset = b.se()? as i8;
    }
    Ok(p)
}

/// One memory management control operation (7.4.3.3), frame pictures only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mmco {
    /// 1: a short-term picture is no longer used for reference.
    ShortUnused { diff_minus1: u32 },
    /// 2: a long-term picture is no longer used for reference.
    LongUnused { long_term_pic_num: u32 },
    /// 3: a short-term picture becomes long-term.
    ShortToLong { diff_minus1: u32, long_term_frame_idx: u32 },
    /// 4: the highest long-term frame index (plus 1).
    MaxLongIdx { plus1: u32 },
    /// 5: everything is unused for reference.
    Clear,
    /// 6: the current picture becomes long-term.
    CurrentToLong { long_term_frame_idx: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SliceHeader {
    pub nal_unit_type: u8,
    pub nal_ref_idc: u8,
    pub first_mb: u32,
    /// slice_type modulo 5: 0 P, 1 B, 2 I, 3 SP, 4 SI.
    pub slice_type: u8,
    pub pps_id: u8,
    pub frame_num: u32,
    pub field_pic: bool,
    pub bottom_field: bool,
    pub idr_pic_id: u32,
    pub poc_lsb: u32,
    pub delta_poc_bottom: i32,
    pub delta_poc: [i32; 2],
    pub redundant_pic_cnt: u32,
    pub no_output_of_prior_pics: bool,
    pub long_term_reference: bool,
    pub adaptive_marking: bool,
    pub mmco: Vec<Mmco>,
}

impl SliceHeader {
    pub fn is_idr(&self) -> bool {
        self.nal_unit_type == nal_type::IDR
    }
    pub fn is_intra(&self) -> bool {
        matches!(self.slice_type, 2 | 4)
    }
}

/// Whether `b` starts another picture than `a` does (7.4.1.2.4).
pub fn is_new_picture(a: &SliceHeader, b: &SliceHeader) -> bool {
    a.frame_num != b.frame_num
        || a.pps_id != b.pps_id
        || a.field_pic != b.field_pic
        || a.bottom_field != b.bottom_field
        || (a.nal_ref_idc != b.nal_ref_idc && (a.nal_ref_idc == 0 || b.nal_ref_idc == 0))
        || a.poc_lsb != b.poc_lsb
        || a.delta_poc_bottom != b.delta_poc_bottom
        || a.delta_poc != b.delta_poc
        || a.is_idr() != b.is_idr()
        || (a.is_idr() && a.idr_pic_id != b.idr_pic_id)
}

/// The slice header after the NAL header byte, with the parameter sets it names found through
/// `pps_of` and `sps_of`.
pub fn slice_header<'a>(b: &mut BitReader, nal_unit_type: u8, nal_ref_idc: u8, pps_of: impl Fn(u8) -> Option<&'a Pps>, sps_of: impl Fn(u8) -> Option<&'a Sps>) -> Result<SliceHeader> {
    let mut h = SliceHeader { nal_unit_type, nal_ref_idc, ..Default::default() };
    h.first_mb = b.ue()?;
    let st = b.ue()?;
    if st > 9 {
        return Err(Error::Invalid("slice_type"));
    }
    h.slice_type = (st % 5) as u8;
    let pps_id = b.ue()?;
    let pps = pps_of(pps_id.min(255) as u8).ok_or(Error::Invalid("slice refers to a missing PPS"))?;
    let sps = sps_of(pps.sps_id).ok_or(Error::Invalid("slice refers to a missing SPS"))?;
    h.pps_id = pps_id as u8;
    if sps.separate_colour_plane {
        b.skip(2)?;
    }
    h.frame_num = b.bits(sps.log2_max_frame_num as u32)?;
    if !sps.frame_mbs_only {
        h.field_pic = b.flag()?;
        if h.field_pic {
            h.bottom_field = b.flag()?;
        }
    }
    if h.is_idr() {
        h.idr_pic_id = b.ue()?;
    }
    if sps.poc_type == 0 {
        h.poc_lsb = b.bits(sps.log2_max_poc_lsb as u32)?;
        if pps.bottom_field_pic_order_in_frame_present && !h.field_pic {
            h.delta_poc_bottom = b.se()?;
        }
    }
    if sps.poc_type == 1 && !sps.delta_pic_order_always_zero {
        h.delta_poc[0] = b.se()?;
        if pps.bottom_field_pic_order_in_frame_present && !h.field_pic {
            h.delta_poc[1] = b.se()?;
        }
    }
    if pps.redundant_pic_cnt_present {
        h.redundant_pic_cnt = b.ue()?;
    }
    let (p_like, b_slice) = (matches!(h.slice_type, 0 | 3 | 1), h.slice_type == 1);
    let (mut n0, mut n1) = (pps.num_ref_idx_l0_default as u32, pps.num_ref_idx_l1_default as u32);
    if b_slice {
        b.skip(1)?; // direct_spatial_mv_pred_flag
    }
    if p_like {
        if b.flag()? {
            n0 = b.ue()? + 1;
            if b_slice {
                n1 = b.ue()? + 1;
            }
        }
        if n0 > 32 || n1 > 32 {
            return Err(Error::Invalid("num_ref_idx_active"));
        }
        // ref_pic_list_modification
        for list in 0..(1 + b_slice as usize) {
            let _ = list;
            if b.flag()? {
                loop {
                    let idc = b.ue()?;
                    match idc {
                        0..=2 => {
                            b.ue()?;
                        }
                        3 => break,
                        _ => return Err(Error::Invalid("modification_of_pic_nums_idc")),
                    }
                }
            }
        }
        if (pps.weighted_pred && matches!(h.slice_type, 0 | 3)) || (pps.weighted_bipred_idc == 1 && b_slice) {
            b.ue()?;
            let chroma = sps.chroma_format_idc != 0 && !sps.separate_colour_plane;
            if chroma {
                b.ue()?;
            }
            for list in 0..(1 + b_slice as usize) {
                for _ in 0..(if list == 0 { n0 } else { n1 }) {
                    if b.flag()? {
                        b.se()?;
                        b.se()?;
                    }
                    if chroma && b.flag()? {
                        for _ in 0..4 {
                            b.se()?;
                        }
                    }
                }
            }
        }
    }
    if nal_ref_idc != 0 {
        if h.is_idr() {
            h.no_output_of_prior_pics = b.flag()?;
            h.long_term_reference = b.flag()?;
        } else {
            h.adaptive_marking = b.flag()?;
            if h.adaptive_marking {
                loop {
                    let op = b.ue()?;
                    h.mmco.push(match op {
                        0 => break,
                        1 => Mmco::ShortUnused { diff_minus1: b.ue()? },
                        2 => Mmco::LongUnused { long_term_pic_num: b.ue()? },
                        3 => {
                            let diff_minus1 = b.ue()?;
                            Mmco::ShortToLong { diff_minus1, long_term_frame_idx: b.ue()? }
                        }
                        4 => Mmco::MaxLongIdx { plus1: b.ue()? },
                        5 => Mmco::Clear,
                        6 => Mmco::CurrentToLong { long_term_frame_idx: b.ue()? },
                        _ => return Err(Error::Invalid("memory_management_control_operation")),
                    });
                    if h.mmco.len() > 66 {
                        return Err(Error::Invalid("too many memory management operations"));
                    }
                }
            }
        }
    }
    Ok(h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_lists_are_complete() {
        assert_eq!(DEFAULT_8X8_INTRA.len(), 64);
        assert_eq!(DEFAULT_8X8_INTER.len(), 64);
        // the default 8x8 lists end on the largest value
        assert_eq!(*DEFAULT_8X8_INTRA.last().unwrap(), 42);
        assert_eq!(*DEFAULT_8X8_INTER.last().unwrap(), 35);
    }

    #[test]
    fn fallback_rules() {
        // list 0 sent explicitly, everything else absent: lists 1 and 2 copy list 0, list 3 falls
        // back on the default inter list
        let mut lists = vec![ListSpec::Absent; 8];
        lists[0] = ListSpec::Explicit(vec![7; 16]);
        let s = ScalingSpec { lists };
        let m = s.resolve(&Matrices::defaults());
        assert_eq!(m.l4[0], [7; 16]);
        assert_eq!(m.l4[2], [7; 16]);
        assert_eq!(m.l4[3], DEFAULT_4X4_INTER);
        assert_eq!(m.l4[5], DEFAULT_4X4_INTER);
        assert_eq!(m.l8[0], DEFAULT_8X8_INTRA);
    }
}
