//! The DXVA 1.0 structures MPEG-1 / MPEG-2 VLD decoding takes (`DXVA_PictureParameters`,
//! `DXVA_QmatrixData`, `DXVA_SliceInfo`, dxva.h, packed to 1 byte), filled from the parsed headers.

use crate::mpeg2::{Matrices, PicType, Picture, Sequence, SliceInfo};

/// `DXVA_PictureParameters`.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct PicParams {
    pub decoded_picture_index: u16,
    pub deblocked_picture_index: u16,
    pub forward_ref_picture_index: u16,
    pub backward_ref_picture_index: u16,
    pub pic_width_in_mb_minus1: u16,
    pub pic_height_in_mb_minus1: u16,
    pub macroblock_width_minus1: u8,
    pub macroblock_height_minus1: u8,
    pub block_width_minus1: u8,
    pub block_height_minus1: u8,
    pub bpp_minus1: u8,
    pub pic_structure: u8,
    pub second_field: u8,
    pub pic_intra: u8,
    pub pic_backward_prediction: u8,
    pub bidirectional_averaging_mode: u8,
    pub mv_precision_and_chroma_relation: u8,
    pub chroma_format: u8,
    pub pic_scan_fixed: u8,
    pub pic_scan_method: u8,
    pub pic_readback_requests: u8,
    pub r_control: u8,
    pub pic_spatial_resid8: u8,
    pub pic_overflow_blocks: u8,
    pub pic_extrapolation: u8,
    pub pic_deblocked: u8,
    pub pic_deblock_confined: u8,
    pub pic_4mv_allowed: u8,
    pub pic_obmc: u8,
    pub pic_bin_pb: u8,
    pub mv_rps: u8,
    pub reserved_bits: u8,
    pub bitstream_fcodes: u16,
    pub bitstream_pce_elements: u16,
    pub bitstream_concealment_need: u8,
    pub bitstream_concealment_method: u8,
}

/// `DXVA_QmatrixData`: four matrices (intra, non-intra, chroma intra, chroma non-intra) in scan
/// order, one `u16` per entry.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct QMatrix {
    pub new_qmatrix: [u8; 4],
    pub qmatrix: [[u16; 64]; 4],
}

/// `DXVA_SliceInfo`.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct SliceInfoDxva {
    pub horizontal_position: u16,
    pub vertical_position: u16,
    pub slice_bits_in_buffer: u32,
    pub slice_data_location: u32,
    pub start_code_bit_offset: u8,
    pub reserved_bits: u8,
    pub mb_bit_offset: u16,
    pub number_mbs_in_slice: u16,
    pub quantizer_scale_code: u16,
    pub bad_slice_chopping: u16,
}

pub fn slice_bytes(v: &[SliceInfoDxva]) -> &[u8] {
    // SAFETY: a packed structure of plain integers, no padding.
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, std::mem::size_of_val(v)) }
}

pub struct PicInfo<'a> {
    pub seq: &'a Sequence,
    pub pic: &'a Picture,
    pub surface: usize,
    pub forward: Option<usize>,
    pub backward: Option<usize>,
    pub second_field: bool,
}

pub fn pic_params(p: &PicInfo) -> PicParams {
    let pic = p.pic;
    let none = 0xffffu16;
    let b = |x: bool| x as u16;
    let fc = pic.f_code;
    let pce = (pic.intra_dc_precision as u16) << 14
        | (pic.structure as u16) << 12
        | b(pic.top_field_first) << 11
        | b(pic.frame_pred_frame_dct) << 10
        | b(pic.concealment_motion_vectors) << 9
        | b(pic.q_scale_type) << 8
        | b(pic.intra_vlc_format) << 7
        | b(pic.alternate_scan) << 6
        | b(pic.repeat_first_field) << 5
        | b(pic.chroma_420_type) << 4
        | b(pic.progressive_frame) << 3;
    PicParams {
        decoded_picture_index: p.surface as u16,
        deblocked_picture_index: 0,
        forward_ref_picture_index: if pic.coding_type == PicType::I { none } else { p.forward.map_or(none, |s| s as u16) },
        backward_ref_picture_index: if pic.coding_type == PicType::B { p.backward.map_or(none, |s| s as u16) } else { none },
        pic_width_in_mb_minus1: p.seq.mb_width() as u16 - 1,
        pic_height_in_mb_minus1: p.seq.mb_height() as u16 - 1,
        macroblock_width_minus1: 15,
        macroblock_height_minus1: 15,
        block_width_minus1: 7,
        block_height_minus1: 7,
        bpp_minus1: 7,
        pic_structure: pic.structure,
        second_field: p.second_field as u8,
        pic_intra: (pic.coding_type == PicType::I) as u8,
        pic_backward_prediction: (pic.coding_type == PicType::B) as u8,
        bidirectional_averaging_mode: 0,
        mv_precision_and_chroma_relation: 0,
        chroma_format: p.seq.chroma_format,
        pic_scan_fixed: 1,
        pic_scan_method: pic.alternate_scan as u8,
        pic_readback_requests: 0,
        r_control: 0,
        pic_spatial_resid8: 1,
        pic_overflow_blocks: 0,
        pic_extrapolation: 0,
        pic_deblocked: 0,
        pic_deblock_confined: 0,
        pic_4mv_allowed: 0,
        pic_obmc: 0,
        pic_bin_pb: 0,
        mv_rps: 0,
        reserved_bits: 0,
        bitstream_fcodes: (fc[0][0] as u16) << 12 | (fc[0][1] as u16) << 8 | (fc[1][0] as u16) << 4 | fc[1][1] as u16,
        bitstream_pce_elements: pce,
        bitstream_concealment_need: 0,
        bitstream_concealment_method: 0,
    }
}

pub fn qmatrix(m: &Matrices) -> QMatrix {
    let mut q = QMatrix { new_qmatrix: [1; 4], qmatrix: [[0; 64]; 4] };
    for k in 0..4 {
        for j in 0..64 {
            q.qmatrix[k][j] = m.0[k][j] as u16;
        }
    }
    q
}

/// The slice control entries for the slices of a picture whose data is packed into one buffer:
/// `locations[i]` is where slice `i` starts in the buffer, `sizes[i]` its length.
pub fn slice_infos(seq: &Sequence, slices: &[SliceInfo], locations: &[u32]) -> Vec<SliceInfoDxva> {
    let mb_w = seq.mb_width();
    let rows = seq.mb_height();
    slices
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let next_row = slices.get(i + 1).map_or(rows, |n| n.row);
            SliceInfoDxva {
                horizontal_position: 0,
                vertical_position: s.row as u16,
                slice_bits_in_buffer: 8 * (s.end - s.start) as u32,
                slice_data_location: locations[i],
                start_code_bit_offset: 0,
                reserved_bits: 0,
                mb_bit_offset: s.mb_bit_offset as u16,
                number_mbs_in_slice: (next_row.saturating_sub(s.row).max(1) * mb_w) as u16,
                quantizer_scale_code: s.quant_code as u16,
                bad_slice_chopping: 0,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_match_dxva_h() {
        assert_eq!(std::mem::size_of::<PicParams>(), 12 + 26 + 4 + 2);
        assert_eq!(std::mem::size_of::<SliceInfoDxva>(), 2 + 2 + 4 + 4 + 1 + 1 + 2 + 2 + 2 + 2);
        assert_eq!(std::mem::size_of::<QMatrix>(), 4 + 512);
    }
}
