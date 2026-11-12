//! Önizleme penceresinin boyut ve konum kuralları (saf hesaplar, mantıksal piksel).

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Yapışma eşiği: çalışma alanı boyutunun bu oranından yakın kenara yapışılır.
const SNAP: f32 = 0.10;

fn round1(v: f32) -> f32 {
    (v * 10.0).round() / 10.0
}

/// İçeriğin istediği boyut, en-boy korunarak çalışma alanının `ratio` katına sığdırılır (büyütülmez).
pub fn fit(preferred: (f32, f32), work: Rect, ratio: f32) -> (f32, f32) {
    let (w, h) = (preferred.0.max(1.0), preferred.1.max(1.0));
    let s = (work.w * ratio / w).min(work.h * ratio / h).min(1.0);
    (round1(w * s), round1(h * s))
}

/// İlk açılış: çalışma alanında ortalanır.
pub fn first_rect(size: (f32, f32), work: Rect) -> Rect {
    clamp(Rect { x: work.x + (work.w - size.0) / 2.0, y: work.y + (work.h - size.1) / 2.0, w: size.0, h: size.1 }, work)
}

/// Pencere yeni boyuta geçerken: eski merkez korunur; eski pencere bir kenara yakınsa o kenara
/// yapışır; sonuç çalışma alanına sıkıştırılır.
pub fn relocate(old: Rect, new_size: (f32, f32), work: Rect) -> Rect {
    let (w, h) = new_size;
    let (cx, cy) = (old.x + old.w / 2.0, old.y + old.h / 2.0);
    let mut r = Rect { x: cx - w / 2.0, y: cy - h / 2.0, w, h };
    let (sx, sy) = (work.w * SNAP, work.h * SNAP);
    if old.x - work.x < sx {
        r.x = work.x;
    } else if work.x + work.w - (old.x + old.w) < sx {
        r.x = work.x + work.w - w;
    }
    if old.y - work.y < sy {
        r.y = work.y;
    } else if work.y + work.h - (old.y + old.h) < sy {
        r.y = work.y + work.h - h;
    }
    clamp(r, work)
}

fn clamp(mut r: Rect, work: Rect) -> Rect {
    r.x = r.x.min(work.x + work.w - r.w).max(work.x);
    r.y = r.y.min(work.y + work.h - r.h).max(work.y);
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }

    #[test] fn fits_to_80_percent() { assert_eq!(fit((3840., 2160.), rect(0.,0.,1920.,1040.), 0.8), (1479.1, 832.0)); }
    #[test] fn small_stays_small() { assert_eq!(fit((400., 300.), rect(0.,0.,1920.,1040.), 0.8), (400., 300.)); }
    #[test] fn centered_first() { assert_eq!(first_rect((800., 600.), rect(0.,0.,1920.,1040.)), rect(560., 220., 800., 600.)); }
    #[test] fn keeps_center() { assert_eq!(relocate(rect(500.,200.,800.,600.), (400.,300.), rect(0.,0.,1920.,1040.)), rect(700.,350.,400.,300.)); }
    #[test] fn snaps_to_right_edge() { let r = relocate(rect(1100.,200.,800.,600.), (1000.,600.), rect(0.,0.,1920.,1040.)); assert_eq!(r.x + r.w, 1920.); }
    #[test] fn clamps_inside() { let r = relocate(rect(1800.,900.,100.,100.), (900.,700.), rect(0.,0.,1920.,1040.)); assert!(r.x >= 0. && r.y >= 0. && r.x + r.w <= 1920. && r.y + r.h <= 1040.); }
}
