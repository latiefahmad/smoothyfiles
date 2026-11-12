//! HEVC picture management: picture order count (ITU-T H.265, 8.3.1), the reference picture
//! set and marking (8.3.2), and the output order with "bumping" (C.5.2). The pictures
//! themselves live on the GPU; this keeps track of which surface of the output texture array
//! holds which picture.

use std::collections::VecDeque;

use crate::hevc::{NalHeader, Sps, nal_type};
use crate::slice::SliceHeader;
use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefMark {
    Unused,
    Short,
    Long,
}

/// A decoded picture in the buffer.
#[derive(Debug, Clone)]
pub struct Pic {
    pub surface: usize,
    pub poc: i32,
    pub pts: i64,
    pub mark: RefMark,
    pub needed_for_output: bool,
    latency: u32,
}

/// A picture ready to be shown, in display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutFrame {
    pub surface: usize,
    pub poc: i32,
    pub pts: i64,
}

/// What the type of a NAL unit says about the picture.
#[derive(Debug, Clone, Copy)]
pub struct PicKind {
    pub nal_unit_type: u8,
    pub temporal_id: u8,
}

impl PicKind {
    pub fn new(n: &NalHeader) -> Self {
        Self { nal_unit_type: n.nal_unit_type, temporal_id: n.temporal_id }
    }
    pub fn is_irap(&self) -> bool {
        (nal_type::BLA_W_LP..=nal_type::RSV_IRAP_23).contains(&self.nal_unit_type)
    }
    pub fn is_idr(&self) -> bool {
        matches!(self.nal_unit_type, nal_type::IDR_W_RADL | nal_type::IDR_N_LP)
    }
    pub fn is_bla(&self) -> bool {
        (nal_type::BLA_W_LP..nal_type::IDR_W_RADL).contains(&self.nal_unit_type)
    }
    pub fn is_rasl(&self) -> bool {
        matches!(self.nal_unit_type, nal_type::RASL_N | nal_type::RASL_R)
    }
    pub fn is_radl(&self) -> bool {
        matches!(self.nal_unit_type, nal_type::RADL_N | nal_type::RADL_R)
    }
    /// Sub-layer non-reference picture (an even type up to 14).
    pub fn is_sub_layer_non_ref(&self) -> bool {
        self.nal_unit_type <= 14 && self.nal_unit_type % 2 == 0
    }
}

/// The reference picture set of the picture about to be decoded: surfaces it predicts from.
#[derive(Debug, Clone, Default)]
pub struct RefSets {
    pub st_curr_before: Vec<usize>,
    pub st_curr_after: Vec<usize>,
    pub lt_curr: Vec<usize>,
}

/// Result of [`Dpb::start_picture`].
#[derive(Debug, Clone)]
pub struct Started {
    pub poc: i32,
    pub sets: RefSets,
}

pub struct Dpb {
    pics: Vec<Pic>,
    pool: usize,
    prev_tid0_poc: i32,
    started: bool,
    after_eos: bool,
    /// NoRaslOutputFlag of the latest IRAP picture.
    irap_no_rasl: bool,
    ready: VecDeque<OutFrame>,
    held: Option<usize>,
}

impl Dpb {
    /// A buffer over `pool` surfaces.
    pub fn new(pool: usize) -> Self {
        Self { pics: Vec::new(), pool, prev_tid0_poc: 0, started: false, after_eos: false, irap_no_rasl: true, ready: VecDeque::new(), held: None }
    }

    /// The pictures kept as reference, in a stable order (what DXVA calls RefPicList).
    pub fn ref_pics(&self) -> Vec<&Pic> {
        self.pics.iter().filter(|p| p.mark != RefMark::Unused).collect()
    }

    /// An end of sequence NAL unit was seen: the next picture starts afresh.
    pub fn end_of_sequence(&mut self) {
        self.after_eos = true;
    }

    /// A surface nothing refers to any more.
    pub fn alloc(&self) -> Option<usize> {
        (0..self.pool).find(|&s| !self.pics.iter().any(|p| p.surface == s) && !self.ready.iter().any(|f| f.surface == s) && self.held != Some(s))
    }

    /// The caller is done with the frame handed out last.
    pub fn release_held(&mut self) {
        self.held = None;
    }

    /// The next picture in display order, if one is ready. It stays valid until
    /// [`Dpb::release_held`].
    pub fn next_output(&mut self) -> Option<OutFrame> {
        let f = self.ready.pop_front()?;
        self.held = Some(f.surface);
        Some(f)
    }

    pub fn has_output(&self) -> bool {
        !self.ready.is_empty()
    }

    /// Outputs the picture with the lowest order count that waits for output (C.5.2.4).
    fn bump(&mut self) -> bool {
        let Some(i) = self.pics.iter().enumerate().filter(|(_, p)| p.needed_for_output).min_by_key(|(_, p)| p.poc).map(|(i, _)| i) else {
            return false;
        };
        let p = &mut self.pics[i];
        p.needed_for_output = false;
        let f = OutFrame { surface: p.surface, poc: p.poc, pts: p.pts };
        if p.mark == RefMark::Unused {
            self.pics.remove(i);
        }
        self.ready.push_back(f);
        true
    }

    fn remove_unused(&mut self) {
        self.pics.retain(|p| p.needed_for_output || p.mark != RefMark::Unused);
    }

    /// End of the stream: everything waiting is output.
    pub fn flush(&mut self) {
        while self.bump() {}
        self.remove_unused();
    }

    fn waiting(&self) -> usize {
        self.pics.iter().filter(|p| p.needed_for_output).count()
    }

    fn latency_exceeded(&self, sps: &Sps) -> bool {
        if sps.max_latency_increase_plus1 == 0 {
            return false;
        }
        let limit = sps.max_num_reorder_pics as u32 + sps.max_latency_increase_plus1 - 1;
        self.pics.iter().any(|p| p.needed_for_output && p.latency >= limit)
    }

    /// Picture order count, reference picture set and marking, and the output the buffer must
    /// make room with, for a picture about to be decoded. `None` when the picture is not
    /// decoded at all (a RASL picture whose IRAP picture starts the stream, or anything before
    /// the first IRAP picture).
    pub fn start_picture(&mut self, kind: PicKind, sh: &SliceHeader, sps: &Sps) -> Result<Option<Started>> {
        let irap = kind.is_irap();
        if !self.started && !irap {
            return Ok(None);
        }
        let no_rasl = irap && (kind.is_idr() || kind.is_bla() || !self.started || self.after_eos);
        if irap {
            self.irap_no_rasl = no_rasl;
        } else if kind.is_rasl() && self.irap_no_rasl {
            return Ok(None);
        }

        // 8.3.1
        let max_lsb = 1i32 << sps.log2_max_poc_lsb;
        let lsb = sh.poc_lsb as i32;
        let msb = if no_rasl {
            0
        } else {
            let prev_lsb = self.prev_tid0_poc & (max_lsb - 1);
            let prev_msb = self.prev_tid0_poc - prev_lsb;
            if lsb < prev_lsb && prev_lsb - lsb >= max_lsb / 2 {
                prev_msb + max_lsb
            } else if lsb > prev_lsb && lsb - prev_lsb > max_lsb / 2 {
                prev_msb - max_lsb
            } else {
                prev_msb
            }
        };
        let poc = msb + lsb;
        if kind.temporal_id == 0 && !kind.is_rasl() && !kind.is_radl() && !kind.is_sub_layer_non_ref() {
            self.prev_tid0_poc = poc;
        }

        // 8.3.2
        let mut sets = RefSets::default();
        if no_rasl {
            for p in &mut self.pics {
                p.mark = RefMark::Unused;
            }
        } else if !kind.is_idr() {
            self.mark_references(sh, poc, max_lsb, &mut sets)?;
        } else {
            for p in &mut self.pics {
                p.mark = RefMark::Unused;
            }
        }

        // C.5.2.2
        if no_rasl && self.started {
            if sh.no_output_of_prior_pics {
                self.pics.clear();
                self.ready.clear();
            } else {
                self.flush();
            }
        } else {
            self.remove_unused();
            let max_dec = sps.max_dec_pic_buffering_minus1 as usize + 1;
            while (self.waiting() > sps.max_num_reorder_pics as usize || self.latency_exceeded(sps) || self.pics.len() >= max_dec) && self.bump() {}
        }
        self.started = true;
        self.after_eos = false;
        Ok(Some(Started { poc, sets }))
    }

    fn mark_references(&mut self, sh: &SliceHeader, poc: i32, max_lsb: i32, sets: &mut RefSets) -> Result<()> {
        // Which picture each entry names, as indexes into `self.pics` (None: missing).
        let mut keep = vec![false; self.pics.len()];
        let mut lt: Vec<(Option<usize>, bool)> = Vec::new();
        for l in &sh.long_term {
            let mut v = l.poc_lsb as i32;
            if l.msb_present {
                v += poc - l.delta_poc_msb_cycle as i32 * max_lsb - (poc & (max_lsb - 1));
            }
            let found = self.pics.iter().position(|p| p.mark != RefMark::Unused && if l.msb_present { p.poc == v } else { (p.poc & (max_lsb - 1)) == v });
            lt.push((found, l.used_by_curr));
            if let Some(i) = found {
                keep[i] = true;
            }
        }
        // The long-term ones are marked before the short-term lookup (they are no longer short-term).
        for &(f, _) in &lt {
            if let Some(i) = f {
                self.pics[i].mark = RefMark::Long;
            }
        }
        let st = &sh.st_rps;
        let find_st = |this: &Self, d: i32, keep: &mut Vec<bool>| {
            let want = poc + d;
            let f = this.pics.iter().position(|p| p.mark == RefMark::Short && p.poc == want);
            if let Some(i) = f {
                keep[i] = true;
            }
            (f, want)
        };
        let mut before = Vec::new();
        let mut after = Vec::new();
        for (i, &d) in st.delta_poc_s0.iter().enumerate() {
            let (f, want) = find_st(self, d, &mut keep);
            if st.used_s0[i] {
                before.push((f, want));
            }
        }
        for (i, &d) in st.delta_poc_s1.iter().enumerate() {
            let (f, want) = find_st(self, d, &mut keep);
            if st.used_s1[i] {
                after.push((f, want));
            }
        }
        for (i, p) in self.pics.iter_mut().enumerate() {
            if !keep[i] {
                p.mark = RefMark::Unused;
            }
        }
        // Pictures that are gone but needed get a surface of their own (undefined content), so
        // a damaged stream still decodes something.
        let resolve = |this: &mut Self, f: Option<usize>, want: i32, mark: RefMark| -> Result<usize> {
            if let Some(i) = f {
                return Ok(this.pics[i].surface);
            }
            let surface = this.alloc().ok_or(Error::Invalid("no free surface for a missing reference picture"))?;
            this.pics.push(Pic { surface, poc: want, pts: 0, mark, needed_for_output: false, latency: 0 });
            Ok(surface)
        };
        for (f, want) in before {
            let s = resolve(self, f, want, RefMark::Short)?;
            sets.st_curr_before.push(s);
        }
        for (f, want) in after {
            let s = resolve(self, f, want, RefMark::Short)?;
            sets.st_curr_after.push(s);
        }
        for (i, (f, used)) in lt.into_iter().enumerate() {
            if used {
                let l = &sh.long_term[i];
                let mut v = l.poc_lsb as i32;
                if l.msb_present {
                    v += poc - l.delta_poc_msb_cycle as i32 * max_lsb - (poc & (max_lsb - 1));
                }
                let s = resolve(self, f, v, RefMark::Long)?;
                sets.lt_curr.push(s);
            }
        }
        Ok(())
    }

    /// The current picture is decoded into `surface`: it joins the buffer, and the additional
    /// bumping of C.5.2.3 runs.
    pub fn finish_picture(&mut self, surface: usize, poc: i32, pts: i64, output: bool, sps: &Sps) {
        if output {
            for p in &mut self.pics {
                if p.needed_for_output {
                    p.latency += 1;
                }
            }
        }
        self.pics.push(Pic { surface, poc, pts, mark: RefMark::Short, needed_for_output: output, latency: 0 });
        while (self.waiting() > sps.max_num_reorder_pics as usize || self.latency_exceeded(sps)) && self.bump() {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hevc::StRps;

    fn sps() -> Sps {
        Sps { log2_max_poc_lsb: 4, max_dec_pic_buffering_minus1: 3, max_num_reorder_pics: 1, ..Default::default() }
    }

    fn header(lsb: u32, before: &[i32]) -> SliceHeader {
        SliceHeader {
            first_slice_segment_in_pic: true,
            no_output_of_prior_pics: false,
            pps_id: 0,
            slice_type: 0,
            pic_output: true,
            poc_lsb: lsb,
            st_rps: StRps { delta_poc_s0: before.to_vec(), used_s0: vec![true; before.len()], ..Default::default() },
            st_rps_sps_idx: None,
            st_rps_bits: 0,
            st_rps_ref_num_delta_pocs: 0,
            long_term: Vec::new(),
        }
    }

    fn kind(t: u8) -> PicKind {
        PicKind { nal_unit_type: t, temporal_id: 0 }
    }

    #[test]
    fn poc_wraps_and_output_is_in_order() {
        let sps = sps();
        let mut d = Dpb::new(6);
        let mut order = Vec::new();
        // IDR, then pictures 1..40 in plain order: the lsb (4 bits) wraps twice.
        for n in 0..40i32 {
            let (k, sh) = if n == 0 { (kind(nal_type::IDR_W_RADL), header(0, &[])) } else { (kind(1), header((n & 15) as u32, &[-1])) };
            let st = d.start_picture(k, &sh, &sps).unwrap().unwrap();
            assert_eq!(st.poc, n);
            let s = d.alloc().unwrap();
            d.finish_picture(s, st.poc, n as i64, true, &sps);
            while let Some(f) = d.next_output() {
                order.push(f.poc);
                d.release_held();
            }
        }
        d.flush();
        while let Some(f) = d.next_output() {
            order.push(f.poc);
            d.release_held();
        }
        assert_eq!(order, (0..40).collect::<Vec<_>>());
    }

    #[test]
    fn reordered_pictures_come_out_sorted() {
        let sps = sps();
        let mut d = Dpb::new(6);
        let mut out = Vec::new();
        // decode order POC 0, 2, 1, 4, 3
        let seq: [(i32, &[i32]); 5] = [(0, &[]), (2, &[-2]), (1, &[-1]), (4, &[-2]), (3, &[-1])];
        for (i, (poc, before)) in seq.iter().enumerate() {
            let (k, sh) = if i == 0 { (kind(nal_type::IDR_W_RADL), header(0, &[])) } else { (kind(1), header(*poc as u32, before)) };
            let st = d.start_picture(k, &sh, &sps).unwrap().unwrap();
            let s = d.alloc().unwrap();
            d.finish_picture(s, st.poc, 0, true, &sps);
            while let Some(f) = d.next_output() {
                out.push(f.poc);
                d.release_held();
            }
        }
        d.flush();
        while let Some(f) = d.next_output() {
            out.push(f.poc);
            d.release_held();
        }
        assert_eq!(out, [0, 1, 2, 3, 4]);
    }

    #[test]
    fn rasl_after_stream_start_cra_is_skipped() {
        let sps = sps();
        let mut d = Dpb::new(6);
        assert!(d.start_picture(kind(nal_type::CRA_NUT), &header(8, &[]), &sps).unwrap().is_some());
        assert!(d.start_picture(kind(nal_type::RASL_N), &header(6, &[-2]), &sps).unwrap().is_none());
        assert!(d.start_picture(kind(nal_type::RADL_N), &header(7, &[]), &sps).unwrap().is_some());
    }
}
