//! VP9 bitstream parts the host has to read for the GPU: the superframe index and the
//! uncompressed header (VP9 bitstream specification, 6.2). Everything after the uncompressed
//! header (the compressed header and the tiles) is entropy coded and left to the video engine.
//!
//! The header carries state from frame to frame: the loop filter deltas, the segmentation
//! features, the colour configuration, and the sizes of the eight reference slots (a frame can
//! take its size from a reference). [`Parser`] keeps those; the decoder calls
//! [`Parser::refresh`] after a frame with the slots the frame replaced.

use crate::bits::BitReader;
use crate::{Error, Result};

/// Splits a packet into its frames (a "superframe": hidden frames and the shown one sent
/// together, with an index of the sizes at the end). A packet without an index is one frame.
pub fn split_superframe(data: &[u8]) -> Result<Vec<&[u8]>> {
    let Some(&last) = data.last() else { return Ok(Vec::new()) };
    if last & 0xe0 == 0xc0 {
        let frames = (last & 7) as usize + 1;
        let mag = ((last >> 3) & 3) as usize + 1;
        let index = 2 + mag * frames;
        if data.len() >= index && data[data.len() - index] == last {
            let body = data.len() - index;
            let mut out = Vec::with_capacity(frames);
            let mut p = 0usize;
            let sizes = &data[body + 1..data.len() - 1];
            for i in 0..frames {
                let mut n = 0usize;
                for k in 0..mag {
                    n |= (sizes[i * mag + k] as usize) << (8 * k);
                }
                if p + n > body {
                    return Err(Error::Invalid("VP9 superframe index runs past the packet"));
                }
                if n > 0 {
                    out.push(&data[p..p + n]);
                }
                p += n;
            }
            return Ok(out);
        }
    }
    Ok(vec![data])
}

/// Segmentation features (`segmentation_params`), persistent between frames.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Segmentation {
    pub enabled: bool,
    pub update_map: bool,
    pub temporal_update: bool,
    pub abs_delta: bool,
    pub tree_probs: [u8; 7],
    pub pred_probs: [u8; 3],
    /// Per segment: alternative quantizer, loop filter level, reference frame, skip.
    pub feature_data: [[i16; 4]; 8],
    pub feature_enabled: [[bool; 4]; 8],
}

/// One uncompressed header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub profile: u8,
    /// `show_existing_frame`: show the picture in this slot and nothing else.
    pub show_existing: Option<u8>,
    pub key_frame: bool,
    pub show_frame: bool,
    pub error_resilient: bool,
    pub intra_only: bool,
    pub reset_frame_context: u8,
    pub refresh_frame_flags: u8,
    /// Slots the three references (last, golden, altref) come from.
    pub ref_frame_idx: [u8; 3],
    /// Sign bias of intra (0), last, golden, altref.
    pub sign_bias: [bool; 4],
    pub allow_high_precision_mv: bool,
    /// 0 smooth, 1 regular, 2 sharp, 3 bilinear, 4 switchable.
    pub interp_filter: u8,
    pub refresh_frame_context: bool,
    pub frame_parallel_decoding: bool,
    pub frame_context_idx: u8,
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    pub subsampling_x: bool,
    pub subsampling_y: bool,
    pub color_space: u8,
    pub full_range: bool,
    pub filter_level: u8,
    pub sharpness: u8,
    pub lf_delta_enabled: bool,
    pub lf_delta_update: bool,
    pub ref_deltas: [i8; 4],
    pub mode_deltas: [i8; 2],
    pub base_qindex: u8,
    pub y_dc_delta_q: i8,
    pub uv_dc_delta_q: i8,
    pub uv_ac_delta_q: i8,
    pub seg: Segmentation,
    pub log2_tile_cols: u8,
    pub log2_tile_rows: u8,
    /// Size of the compressed header that follows the uncompressed one.
    pub compressed_header_size: u16,
    /// Size of the uncompressed header in bytes (byte aligned).
    pub uncompressed_size: u32,
}

impl Header {
    /// Intra only pictures: key frames and `intra_only` ones (no references used).
    pub fn is_intra(&self) -> bool {
        self.key_frame || self.intra_only
    }
}

/// The state the header syntax depends on.
#[derive(Debug, Clone)]
pub struct Parser {
    /// Width and height of the picture in each reference slot (0 when empty).
    pub ref_dims: [(u32, u32); 8],
    ref_deltas: [i8; 4],
    mode_deltas: [i8; 2],
    seg: Segmentation,
    bit_depth: u8,
    subsampling: (bool, bool),
    color_space: u8,
    full_range: bool,
    have_color: bool,
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

const SYNC: [u32; 3] = [0x49, 0x83, 0x42];

fn su(b: &mut BitReader, n: u32) -> Result<i32> {
    let v = b.bits(n)? as i32;
    Ok(if b.flag()? { -v } else { v })
}

impl Parser {
    pub fn new() -> Self {
        Self { ref_dims: [(0, 0); 8], ref_deltas: [1, 0, -1, -1], mode_deltas: [0; 2], seg: Segmentation::default(), bit_depth: 8, subsampling: (true, true), color_space: 2, full_range: false, have_color: false }
    }

    /// Forgets everything (after a seek: the next frame is a key frame).
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// The slots a frame replaced (`refresh_frame_flags`) now hold a picture of `w` x `h`.
    pub fn refresh(&mut self, flags: u8, w: u32, h: u32) {
        for (i, d) in self.ref_dims.iter_mut().enumerate() {
            if flags >> i & 1 != 0 {
                *d = (w, h);
            }
        }
    }

    fn past_independence(&mut self) {
        self.seg.feature_data = [[0; 4]; 8];
        self.seg.feature_enabled = [[false; 4]; 8];
        self.seg.abs_delta = false;
        self.ref_deltas = [1, 0, -1, -1];
        self.mode_deltas = [0; 2];
    }

    fn color_config(&mut self, b: &mut BitReader, profile: u8) -> Result<()> {
        self.bit_depth = if profile >= 2 { if b.flag()? { 12 } else { 10 } } else { 8 };
        self.color_space = b.bits(3)? as u8;
        if self.color_space != 7 {
            self.full_range = b.flag()?;
            if profile == 1 || profile == 3 {
                self.subsampling = (b.flag()?, b.flag()?);
                b.flag()?;
            } else {
                self.subsampling = (true, true);
            }
        } else if profile == 1 || profile == 3 {
            self.full_range = true;
            self.subsampling = (false, false);
            b.flag()?;
        } else {
            return Err(Error::Invalid("VP9 RGB in profile 0 or 2"));
        }
        self.have_color = true;
        Ok(())
    }

    /// Reads the uncompressed header of one frame (not a superframe: see
    /// [`split_superframe`]).
    pub fn parse(&mut self, data: &[u8]) -> Result<Header> {
        let mut b = BitReader::new(data);
        if b.bits(2)? != 2 {
            return Err(Error::Invalid("VP9 frame marker"));
        }
        let low = b.bits(1)?;
        let high = b.bits(1)?;
        let profile = (high << 1 | low) as u8;
        if profile == 3 && b.flag()? {
            return Err(Error::Unsupported("VP9 profile with reserved bit"));
        }
        let mut h = Header {
            profile,
            show_existing: None,
            key_frame: false,
            show_frame: true,
            error_resilient: false,
            intra_only: false,
            reset_frame_context: 0,
            refresh_frame_flags: 0,
            ref_frame_idx: [0; 3],
            sign_bias: [false; 4],
            allow_high_precision_mv: false,
            interp_filter: 0,
            refresh_frame_context: false,
            frame_parallel_decoding: true,
            frame_context_idx: 0,
            width: 0,
            height: 0,
            bit_depth: self.bit_depth,
            subsampling_x: self.subsampling.0,
            subsampling_y: self.subsampling.1,
            color_space: self.color_space,
            full_range: self.full_range,
            filter_level: 0,
            sharpness: 0,
            lf_delta_enabled: false,
            lf_delta_update: false,
            ref_deltas: self.ref_deltas,
            mode_deltas: self.mode_deltas,
            base_qindex: 0,
            y_dc_delta_q: 0,
            uv_dc_delta_q: 0,
            uv_ac_delta_q: 0,
            seg: self.seg,
            log2_tile_cols: 0,
            log2_tile_rows: 0,
            compressed_header_size: 0,
            uncompressed_size: 0,
        };
        if b.flag()? {
            let idx = b.bits(3)? as u8;
            let (w, hh) = self.ref_dims[idx as usize];
            if w == 0 {
                return Err(Error::Invalid("VP9 show_existing_frame of an empty slot"));
            }
            h.show_existing = Some(idx);
            h.width = w;
            h.height = hh;
            h.uncompressed_size = (b.position() as u32).div_ceil(8);
            return Ok(h);
        }
        h.key_frame = !b.flag()?;
        h.show_frame = b.flag()?;
        h.error_resilient = b.flag()?;
        if h.key_frame {
            self.sync(&mut b)?;
            self.color_config(&mut b, profile)?;
            Self::frame_size(&mut b, &mut h)?;
            Self::render_size(&mut b)?;
            h.refresh_frame_flags = 0xff;
        } else {
            h.intra_only = if h.show_frame { false } else { b.flag()? };
            h.reset_frame_context = if h.error_resilient { 0 } else { b.bits(2)? as u8 };
            if h.intra_only {
                self.sync(&mut b)?;
                if profile > 0 {
                    self.color_config(&mut b, profile)?;
                } else {
                    self.color_space = 1;
                    self.subsampling = (true, true);
                    self.bit_depth = 8;
                    self.have_color = true;
                }
                h.refresh_frame_flags = b.bits(8)? as u8;
                Self::frame_size(&mut b, &mut h)?;
                Self::render_size(&mut b)?;
            } else {
                if !self.have_color {
                    return Err(Error::Invalid("VP9 inter frame before any key frame"));
                }
                h.refresh_frame_flags = b.bits(8)? as u8;
                for i in 0..3 {
                    h.ref_frame_idx[i] = b.bits(3)? as u8;
                    h.sign_bias[i + 1] = b.flag()?;
                }
                // frame_size_with_refs
                let mut found = false;
                for i in 0..3 {
                    if b.flag()? {
                        let (w, hh) = self.ref_dims[h.ref_frame_idx[i] as usize];
                        if w == 0 {
                            return Err(Error::Invalid("VP9 frame sized from an empty reference slot"));
                        }
                        h.width = w;
                        h.height = hh;
                        found = true;
                        break;
                    }
                }
                if !found {
                    Self::frame_size(&mut b, &mut h)?;
                }
                Self::render_size(&mut b)?;
                h.allow_high_precision_mv = b.flag()?;
                h.interp_filter = if b.flag()? { 4 } else { b.bits(2)? as u8 };
            }
        }
        h.bit_depth = self.bit_depth;
        h.subsampling_x = self.subsampling.0;
        h.subsampling_y = self.subsampling.1;
        h.color_space = self.color_space;
        h.full_range = self.full_range;
        if !h.error_resilient {
            h.refresh_frame_context = b.flag()?;
            h.frame_parallel_decoding = b.flag()?;
        }
        h.frame_context_idx = b.bits(2)? as u8;
        if h.is_intra() || h.error_resilient {
            self.past_independence();
            // the probability contexts are reset as reset_frame_context says; the one used is 0
            h.frame_context_idx = 0;
        }
        // loop_filter_params
        h.filter_level = b.bits(6)? as u8;
        h.sharpness = b.bits(3)? as u8;
        h.lf_delta_enabled = b.flag()?;
        if h.lf_delta_enabled {
            h.lf_delta_update = b.flag()?;
            if h.lf_delta_update {
                for i in 0..4 {
                    if b.flag()? {
                        self.ref_deltas[i] = su(&mut b, 6)? as i8;
                    }
                }
                for i in 0..2 {
                    if b.flag()? {
                        self.mode_deltas[i] = su(&mut b, 6)? as i8;
                    }
                }
            }
        }
        h.ref_deltas = self.ref_deltas;
        h.mode_deltas = self.mode_deltas;
        // quantization_params
        h.base_qindex = b.bits(8)? as u8;
        let delta = |b: &mut BitReader| -> Result<i8> { Ok(if b.flag()? { su(b, 4)? as i8 } else { 0 }) };
        h.y_dc_delta_q = delta(&mut b)?;
        h.uv_dc_delta_q = delta(&mut b)?;
        h.uv_ac_delta_q = delta(&mut b)?;
        // segmentation_params
        self.seg.enabled = b.flag()?;
        self.seg.update_map = false;
        self.seg.temporal_update = false;
        if self.seg.enabled {
            self.seg.update_map = b.flag()?;
            if self.seg.update_map {
                for i in 0..7 {
                    self.seg.tree_probs[i] = if b.flag()? { b.bits(8)? as u8 } else { 255 };
                }
                self.seg.temporal_update = b.flag()?;
                for i in 0..3 {
                    self.seg.pred_probs[i] = if self.seg.temporal_update && b.flag()? { b.bits(8)? as u8 } else { 255 };
                }
            }
            if b.flag()? {
                self.seg.abs_delta = b.flag()?;
                self.seg.feature_data = [[0; 4]; 8];
                self.seg.feature_enabled = [[false; 4]; 8];
                const BITS: [u32; 4] = [8, 6, 2, 0];
                const SIGNED: [bool; 4] = [true, true, false, false];
                for i in 0..8 {
                    for j in 0..4 {
                        if b.flag()? {
                            self.seg.feature_enabled[i][j] = true;
                            let mut v = b.bits(BITS[j])? as i16;
                            if SIGNED[j] && b.flag()? {
                                v = -v;
                            }
                            self.seg.feature_data[i][j] = v;
                        }
                    }
                }
            }
        }
        h.seg = self.seg;
        // tile_info
        let sb_cols = (h.width.div_ceil(8)).div_ceil(8);
        let mut min_log2 = 0u32;
        while (64u32 << min_log2) < sb_cols {
            min_log2 += 1;
        }
        let mut max_log2 = 1u32;
        while (sb_cols >> max_log2) >= 4 {
            max_log2 += 1;
        }
        max_log2 -= 1;
        let mut cols = min_log2;
        while cols < max_log2 {
            if b.flag()? {
                cols += 1;
            } else {
                break;
            }
        }
        h.log2_tile_cols = cols as u8;
        h.log2_tile_rows = if b.flag()? { 1 + b.bits(1)? as u8 } else { 0 };
        h.compressed_header_size = b.bits(16)? as u16;
        if h.compressed_header_size == 0 {
            return Err(Error::Invalid("VP9 empty compressed header"));
        }
        h.uncompressed_size = (b.position() as u32).div_ceil(8);
        if h.width == 0 || h.height == 0 {
            return Err(Error::Invalid("VP9 frame size"));
        }
        Ok(h)
    }

    fn sync(&self, b: &mut BitReader) -> Result<()> {
        for s in SYNC {
            if b.bits(8)? != s {
                return Err(Error::Invalid("VP9 frame sync code"));
            }
        }
        Ok(())
    }

    fn frame_size(b: &mut BitReader, h: &mut Header) -> Result<()> {
        h.width = b.bits(16)? + 1;
        h.height = b.bits(16)? + 1;
        Ok(())
    }

    fn render_size(b: &mut BitReader) -> Result<()> {
        if b.flag()? {
            b.bits(16)?;
            b.bits(16)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn superframe_index() {
        // two frames of 3 and 2 bytes, one byte sizes: marker 0b1100_0001
        let mut d = vec![1, 2, 3, 4, 5];
        let marker = 0xc0 | 1;
        d.extend([marker, 3, 2, marker]);
        let f = split_superframe(&d).unwrap();
        assert_eq!(f, vec![&[1u8, 2, 3][..], &[4, 5][..]]);
        // no index: the packet is one frame
        assert_eq!(split_superframe(&[0x82, 1, 2]).unwrap().len(), 1);
        // an index that claims more than the packet holds
        let mut d = vec![1, 2];
        d.extend([marker, 9, 9, marker]);
        assert!(split_superframe(&d).is_err());
    }

    #[test]
    fn show_existing_needs_a_picture() {
        let mut p = Parser::new();
        // marker 2, profile 0, show_existing 1, slot 3
        let byte = 0b1000_1011u8;
        assert!(p.parse(&[byte, 0]).is_err());
        p.refresh(0xff, 64, 48);
        let h = p.parse(&[byte, 0]).unwrap();
        assert_eq!((h.show_existing, h.width, h.height), (Some(3), 64, 48));
    }
}
