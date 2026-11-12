//! Resim görüntüleyicinin yakınlaştırma hesapları (saf).

pub const MIN: f32 = 0.1;
pub const MAX: f32 = 3.0;
const FACTOR: f32 = 1.1;

/// Tekerlek: bir çentik %10 (kesirli çentikler orantılı); sığdırma ve %100 arada kalırsa ona
/// yapışır; sınırlar içinde kalır.
pub fn step(zoom: f32, fit: f32, notches: f32) -> f32 {
    let up = notches > 0.0;
    let next = zoom * FACTOR.powf(notches);
    let (lo, hi) = if up { (zoom, next) } else { (next, zoom) };
    // yukarı: önce küçük olan yakalanır; aşağı: önce büyük olan
    let mut snaps = [fit, 1.0];
    snaps.sort_by(|a, b| if up { a.total_cmp(b) } else { b.total_cmp(a) });
    let target = snaps.into_iter().find(|s| *s > lo + 1e-6 && *s < hi - 1e-6).unwrap_or(next);
    target.clamp(MIN.min(fit), MAX)
}

/// Sığdırma: resim alana sığar, küçük resim büyütülmez.
pub fn fit_zoom(image: (f32, f32), area: (f32, f32)) -> f32 {
    (area.0 / image.0.max(1.0)).min(area.1 / image.1.max(1.0)).min(1.0)
}

/// Yakınlaştırma değişince imlecin altındaki resim noktası yerinde kalsın diye yeni konum.
pub fn zoom_offset(offset: (f32, f32), cursor: (f32, f32), old: f32, new: f32) -> (f32, f32) {
    let k = new / old;
    (cursor.0 - (cursor.0 - offset.0) * k, cursor.1 - (cursor.1 - offset.1) * k)
}

/// Alandan küçük eksende resim ortalanır; büyük eksende kenar alanın içine kaçmaz.
pub fn clamp_offset(offset: (f32, f32), shown: (f32, f32), area: (f32, f32)) -> (f32, f32) {
    let axis = |o: f32, s: f32, a: f32| if s <= a { ((a - s) / 2.0).round() } else { o.clamp(a - s, 0.0) };
    (axis(offset.0, shown.0, area.0), axis(offset.1, shown.1, area.1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wheel_steps_ten_percent() {
        assert!((step(1.0, 0.5, 1.0) - 1.1).abs() < 1e-6);
        assert!((step(1.0, 0.5, -1.0) - 1.0 / 1.1).abs() < 1e-6);
    }

    #[test]
    fn partial_notches_zoom_proportionally() {
        // yumuşak kaydırma bir çentiği üç parçada verir: toplam yine %10
        let z = step(step(step(1.2, 0.5, 1.0 / 3.0), 0.5, 1.0 / 3.0), 0.5, 1.0 / 3.0);
        assert!((z - 1.2 * 1.1).abs() < 1e-4, "{z}");
    }

    #[test]
    fn snaps_to_fit_and_actual_size() {
        assert_eq!(step(0.95, 0.5, 1.0), 1.0);
        assert_eq!(step(1.05, 0.5, -1.0), 1.0);
        assert_eq!(step(0.48, 0.5, 1.0), 0.5);
        assert_eq!(step(0.52, 0.5, -1.0), 0.5);
    }

    #[test]
    fn limits() {
        assert_eq!(step(2.9, 0.5, 1.0), MAX);
        assert_eq!(step(0.105, 0.5, -1.0), MIN);
        // dev resim: sığdırma 0,1'in altındaysa alt sınır sığdırmadır
        assert_eq!(step(0.042, 0.04, -1.0), 0.04);
    }

    #[test]
    fn fit_never_upscales() {
        assert_eq!(fit_zoom((100.0, 50.0), (800.0, 600.0)), 1.0);
        assert!((fit_zoom((4000.0, 2000.0), (800.0, 600.0)) - 0.2).abs() < 1e-6);
    }

    #[test]
    fn zoom_keeps_point_under_cursor() {
        // görüntü (0,0)'da 1x; imleç (100,100); 2x'e çıkınca imlecin altındaki resim noktası aynı kalır
        let o = zoom_offset((0.0, 0.0), (100.0, 100.0), 1.0, 2.0);
        assert_eq!(o, (-100.0, -100.0));
    }

    #[test]
    fn small_image_is_centered_and_large_is_clamped() {
        assert_eq!(clamp_offset((5.0, 5.0), (200.0, 100.0), (800.0, 600.0)), (300.0, 250.0));
        // büyük resim kenardan içeri kaçamaz
        assert_eq!(clamp_offset((50.0, -5000.0), (1600.0, 1200.0), (800.0, 600.0)), (0.0, -600.0));
    }
}
