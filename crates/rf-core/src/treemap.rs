//! Squarified treemap layout (Bruls, Huizing, van Wijk): items become rectangles whose areas
//! are proportional to their weights and whose shapes stay as close to squares as possible, so
//! labels fit and small items stay visible.

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub fn new(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }

    pub fn area(&self) -> f32 {
        self.w * self.h
    }

    /// The rectangle shrunk by `d` on every side (never below zero size).
    pub fn inset(&self, d: f32) -> Rect {
        let w = (self.w - 2.0 * d).max(0.0);
        let h = (self.h - 2.0 * d).max(0.0);
        Rect { x: self.x + (self.w - w) / 2.0, y: self.y + (self.h - h) / 2.0, w, h }
    }
}

/// Worst aspect ratio (>= 1) of a row of areas laid along a side of length `side`.
fn worst(row: &[f64], sum: f64, side: f64) -> f64 {
    if sum <= 0.0 || side <= 0.0 {
        return f64::MAX;
    }
    let (mut min, mut max) = (f64::MAX, 0.0f64);
    for &a in row {
        min = min.min(a);
        max = max.max(a);
    }
    let s2 = side * side;
    ((s2 * max) / (sum * sum)).max((sum * sum) / (s2 * min))
}

/// Lays `weights` out inside `bounds`. `weights` should be sorted descending (the algorithm
/// still works otherwise, with worse shapes); zero weights get an empty rectangle.
/// The result has one rectangle per weight, in the same order. The rectangles tile `bounds`
/// without overlap; their areas are proportional to the weights.
pub fn squarify(weights: &[u64], bounds: Rect) -> Vec<Rect> {
    let mut out = vec![Rect::new(bounds.x, bounds.y, 0.0, 0.0); weights.len()];
    let total: f64 = weights.iter().map(|&w| w as f64).sum();
    if total <= 0.0 || bounds.w <= 0.0 || bounds.h <= 0.0 {
        return out;
    }
    let scale = bounds.area() as f64 / total;
    // work in f64 so long lists of tiny items do not drift
    let (mut x, mut y, mut w, mut h) = (bounds.x as f64, bounds.y as f64, bounds.w as f64, bounds.h as f64);
    let areas: Vec<f64> = weights.iter().map(|&v| v as f64 * scale).collect();
    let n = areas.iter().position(|&a| a <= 0.0).unwrap_or(areas.len());
    let mut i = 0;
    while i < n {
        // the row goes along the shorter side
        let side = w.min(h);
        let mut j = i + 1;
        let mut sum = areas[i];
        while j < n {
            let next = sum + areas[j];
            if worst(&areas[i..=j], next, side) <= worst(&areas[i..j], sum, side) {
                sum = next;
                j += 1;
            } else {
                break;
            }
        }
        // thickness of the row; the last row takes what is left exactly
        let last = j == n;
        if w >= h {
            // a column on the left, items stacked top to bottom
            let cw = if last { w } else { (sum / h).min(w) };
            let mut cy = y;
            for k in i..j {
                let ch = if k == j - 1 { y + h - cy } else { areas[k] / cw };
                out[k] = Rect::new(x as f32, cy as f32, cw as f32, ch as f32);
                cy += ch;
            }
            x += cw;
            w -= cw;
        } else {
            let rh = if last { h } else { (sum / w).min(h) };
            let mut cx = x;
            for k in i..j {
                let cw = if k == j - 1 { x + w - cx } else { areas[k] / rh };
                out[k] = Rect::new(cx as f32, y as f32, cw as f32, rh as f32);
                cx += cw;
            }
            y += rh;
            h -= rh;
        }
        i = j;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overlap(a: &Rect, b: &Rect) -> f32 {
        let w = (a.x + a.w).min(b.x + b.w) - a.x.max(b.x);
        let h = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y);
        if w > 0.0 && h > 0.0 { w * h } else { 0.0 }
    }

    fn check(weights: &[u64], bounds: Rect) {
        let r = squarify(weights, bounds);
        assert_eq!(r.len(), weights.len());
        let total: u64 = weights.iter().sum();
        let mut sum = 0.0;
        for (i, (rc, &w)) in r.iter().zip(weights).enumerate() {
            let want = bounds.area() * (w as f32 / total as f32);
            assert!((rc.area() - want).abs() <= want * 0.001 + 0.05, "item {i}: area {} want {want}", rc.area());
            assert!(rc.x >= bounds.x - 0.01 && rc.y >= bounds.y - 0.01);
            assert!(rc.x + rc.w <= bounds.x + bounds.w + 0.01 && rc.y + rc.h <= bounds.y + bounds.h + 0.01);
            sum += rc.area();
        }
        assert!((sum - bounds.area()).abs() <= bounds.area() * 0.001, "fill: {sum} of {}", bounds.area());
        for a in 0..r.len() {
            for b in a + 1..r.len() {
                assert!(overlap(&r[a], &r[b]) <= 0.5, "overlap {a}/{b}: {:?} {:?}", r[a], r[b]);
            }
        }
    }

    #[test]
    fn fills_the_area_without_overlap() {
        check(&[6, 6, 4, 3, 2, 2, 1], Rect::new(0.0, 0.0, 600.0, 400.0));
        check(&[100, 50, 25, 12, 6, 3, 1, 1, 1], Rect::new(10.0, 20.0, 300.0, 700.0));
        check(&[1], Rect::new(0.0, 0.0, 50.0, 80.0));
        let many: Vec<u64> = (1..=500u64).rev().collect();
        check(&many, Rect::new(0.0, 0.0, 1000.0, 600.0));
    }

    #[test]
    fn zero_weights_and_empty_bounds() {
        let r = squarify(&[10, 5, 0, 0], Rect::new(0.0, 0.0, 100.0, 100.0));
        assert_eq!(r[2].area(), 0.0);
        assert!((r[0].area() + r[1].area() - 10000.0).abs() < 1.0);
        assert!(squarify(&[], Rect::new(0.0, 0.0, 10.0, 10.0)).is_empty());
        assert!(squarify(&[1, 2], Rect::new(0.0, 0.0, 0.0, 10.0)).iter().all(|r| r.area() == 0.0));
    }

    #[test]
    fn shapes_stay_reasonable() {
        let r = squarify(&[6, 6, 4, 3, 2, 2, 1], Rect::new(0.0, 0.0, 600.0, 400.0));
        for rc in &r {
            let ar = (rc.w / rc.h).max(rc.h / rc.w);
            assert!(ar < 4.0, "{rc:?}");
        }
    }
}
