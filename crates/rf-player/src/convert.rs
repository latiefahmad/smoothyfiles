//! Kareyi istenen boyuta ölçekleyerek BGRA'ya çevirir (CPU, satırlar paralel). Parlaklık çift
//! doğrusal, renk en yakın örnekle; BT.601/709/2020, sınırlı/tam aralık, 8–16 bit.

use filmcraft_color::{Matrix, Range};
use rayon::prelude::*;

use crate::source::{Frame, Planes};

struct Coeffs {
    y_off: f32,
    y_scale: f32,
    c_off: f32,
    c_scale: f32,
    rv: f32,
    gu: f32,
    gv: f32,
    bu: f32,
}

fn coeffs(frame: &Frame, bits: u32) -> Coeffs {
    let (kr, kb) = match frame.color.matrix {
        Matrix::Bt601 => Matrix::Bt601.kr_kb(),
        Matrix::Bt709 => Matrix::Bt709.kr_kb(),
        Matrix::Bt2020Ncl => Matrix::Bt2020Ncl.kr_kb(),
    };
    let kg = 1.0 - kr - kb;
    let max = ((1u32 << bits.clamp(8, 16)) - 1) as f32;
    let unit = (1u32 << bits.clamp(8, 16).saturating_sub(8)) as f32 / max;
    let (y_off, y_scale, c_off, c_scale) = match frame.color.range {
        Range::Limited => (16.0 * unit, 1.0 / (219.0 * unit), 128.0 * unit, 1.0 / (224.0 * unit)),
        Range::Full => (0.0, 1.0, 128.0 * unit, 1.0),
    };
    Coeffs { y_off, y_scale, c_off, c_scale, rv: 2.0 * (1.0 - kr), gu: -2.0 * kb * (1.0 - kb) / kg, gv: -2.0 * kr * (1.0 - kr) / kg, bu: 2.0 * (1.0 - kb) }
}

#[inline]
fn u8c(v: f32) -> u8 {
    (v * 255.0 + 0.5).clamp(0.0, 255.0) as u8
}

/// Düzlemden normalleştirilmiş örnek okuyucu (boyutlar önceden doğrulanır).
trait Plane: Sync {
    fn len(&self) -> usize;
    fn at(&self, i: usize) -> f32;
}

struct P8<'a>(&'a [u8]);
struct P16<'a>(&'a [u16], f32);

impl Plane for P8<'_> {
    fn len(&self) -> usize {
        self.0.len()
    }
    #[inline(always)]
    fn at(&self, i: usize) -> f32 {
        // SAFETY: çağıran `i < len` olduğunu doğruladı
        unsafe { *self.0.get_unchecked(i) as f32 * (1.0 / 255.0) }
    }
}

impl Plane for P16<'_> {
    fn len(&self) -> usize {
        self.0.len()
    }
    #[inline(always)]
    fn at(&self, i: usize) -> f32 {
        // SAFETY: çağıran `i < len` olduğunu doğruladı
        unsafe { *self.0.get_unchecked(i) as f32 * self.1 }
    }
}

#[allow(clippy::too_many_arguments)]
fn yuv<P: Plane>(frame: &Frame, y: &P, u: &P, v: &P, shifts: (u32, u32), c: &Coeffs, w: u32, h: u32) -> Vec<u8> {
    let (fw, fh) = (frame.width as usize, frame.height as usize);
    let (sx, sy) = shifts;
    let (cw, ch) = ((fw + (1 << sx) - 1) >> sx, (fh + (1 << sy) - 1) >> sy);
    let mut out = vec![0u8; (w * h * 4) as usize];
    if fw == 0 || fh == 0 || y.len() < fw * fh || u.len() < cw * ch || v.len() < cw * ch {
        return out;
    }
    let (kx, ky) = (fw as f32 / w as f32, fh as f32 / h as f32);
    // sütun başına: (x0, x1, ağırlık, renk sütunu)
    let cols: Vec<(usize, usize, f32, usize)> = (0..w as usize)
        .map(|ox| {
            let fx = ((ox as f32 + 0.5) * kx - 0.5).clamp(0.0, (fw - 1) as f32);
            let x0 = fx as usize;
            let cx = ((((ox as f32 + 0.5) * kx) as usize).min(fw - 1)) >> sx;
            (x0, (x0 + 1).min(fw - 1), fx - x0 as f32, cx.min(cw - 1))
        })
        .collect();
    out.par_chunks_mut((w * 4) as usize).enumerate().for_each(|(oy, row)| {
        let fy = ((oy as f32 + 0.5) * ky - 0.5).clamp(0.0, (fh - 1) as f32);
        let y0 = fy as usize;
        let (r0, r1) = (y0 * fw, (y0 + 1).min(fh - 1) * fw);
        let ty = fy - y0 as f32;
        let crow = (((((oy as f32 + 0.5) * ky) as usize).min(fh - 1)) >> sy).min(ch - 1) * cw;
        for (px, &(x0, x1, tx, cx)) in row.chunks_exact_mut(4).zip(cols.iter()) {
            let top = y.at(r0 + x0) + (y.at(r0 + x1) - y.at(r0 + x0)) * tx;
            let bot = y.at(r1 + x0) + (y.at(r1 + x1) - y.at(r1 + x0)) * tx;
            let luma = (top + (bot - top) * ty - c.y_off) * c.y_scale;
            let cb = (u.at(crow + cx) - c.c_off) * c.c_scale;
            let cr = (v.at(crow + cx) - c.c_off) * c.c_scale;
            px[0] = u8c(luma + c.bu * cb);
            px[1] = u8c(luma + c.gu * cb + c.gv * cr);
            px[2] = u8c(luma + c.rv * cr);
            px[3] = 255;
        }
    });
    out
}

/// `frame`'i `w`×`h` BGRA'ya çevirir (en-boy oranını çağıran korur).
pub fn to_bgra(frame: &Frame, w: u32, h: u32) -> Vec<u8> {
    let (w, h) = (w.max(1), h.max(1));
    match &frame.planes {
        Planes::Yuv8 { planes, chroma, .. } => {
            let c = coeffs(frame, 8);
            yuv(frame, &P8(&planes[0]), &P8(&planes[1]), &P8(&planes[2]), chroma.shifts(), &c, w, h)
        }
        Planes::Yuv16 { planes, chroma, bits, .. } => {
            let c = coeffs(frame, *bits);
            let k = 1.0 / ((1u32 << (*bits).clamp(8, 16)) - 1) as f32;
            yuv(frame, &P16(&planes[0], k), &P16(&planes[1], k), &P16(&planes[2], k), chroma.shifts(), &c, w, h)
        }
        Planes::Rgba8(d) => rgba(frame, w, h, |i| [d[i] as f32 / 255.0, d[i + 1] as f32 / 255.0, d[i + 2] as f32 / 255.0]),
        Planes::RgbaF32(d) => rgba(frame, w, h, |i| {
            let a = d[i + 3].max(1e-5);
            [(d[i] / a).max(0.0).powf(1.0 / 2.2), (d[i + 1] / a).max(0.0).powf(1.0 / 2.2), (d[i + 2] / a).max(0.0).powf(1.0 / 2.2)]
        }),
    }
}

fn rgba(frame: &Frame, w: u32, h: u32, at: impl Fn(usize) -> [f32; 3] + Sync) -> Vec<u8> {
    let (fw, fh) = (frame.width as usize, frame.height as usize);
    let mut out = vec![0u8; (w * h * 4) as usize];
    out.par_chunks_mut((w * 4) as usize).enumerate().for_each(|(oy, row)| {
        let sy = (((oy as f32 + 0.5) * fh as f32 / h as f32) as usize).min(fh - 1);
        for ox in 0..w as usize {
            let sx = (((ox as f32 + 0.5) * fw as f32 / w as f32) as usize).min(fw - 1);
            let [r, g, b] = at((sy * fw + sx) * 4);
            let o = ox * 4;
            row[o] = u8c(b);
            row[o + 1] = u8c(g);
            row[o + 2] = u8c(r);
            row[o + 3] = 255;
        }
    });
    out
}
