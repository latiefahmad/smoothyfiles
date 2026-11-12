//! The "buy me a coffee" strip's messages. Some are plain thanks; the others quote what the
//! speed test measured on this PC (Settings → Speed test): how much faster smoothyfiles opened a
//! big folder, copied or unzipped than Windows, how much less memory it took. A comparison is
//! only said when it was measured here and the difference is real (at least 1.3×); nothing is
//! estimated.

use std::sync::Mutex;

use rf_win::bench::AppKind;

use crate::bench::{OpenTest, Results, Versus};
use crate::t;

/// Below this, "faster" / "less memory" is not worth saying.
const MIN_RATIO: f64 = 1.3;

/// What the last speed test on this PC found, worked out once in the background.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Facts {
    /// Opening the biggest test folder: (files, times faster than File Explorer).
    pub open: Option<(u32, f64)>,
    /// Times less memory than File Explorer showing that folder (Task Manager's figure).
    pub memory: Option<f64>,
    /// Copying (small files or one big file, the better of the two), times faster than Windows.
    pub copy: Option<f64>,
    /// Extracting a ZIP, times faster than Windows.
    pub unzip: Option<f64>,
    /// Searching the test folders by name: (ms, entries looked at).
    pub search: Option<(f64, u64)>,
}

static FACTS: Mutex<Option<Facts>> = Mutex::new(None);

/// Reads the saved speed test on a helper thread (called at start-up and after a test).
pub fn refresh() {
    std::thread::Builder::new()
        .name("rf-donate".into())
        .spawn(|| {
            let f = crate::bench::last_results().map(|r| facts(&r)).unwrap_or_default();
            *FACTS.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
        })
        .ok();
}

fn ratio(slow: f64, fast: f64) -> Option<f64> {
    (fast > 0.0 && slow / fast >= MIN_RATIO).then_some(slow / fast)
}

fn versus(v: &Option<Versus>) -> Option<f64> {
    let v = v.as_ref()?;
    ratio(v.windows_ms?, v.ours_ms?)
}

/// The comparisons a saved result supports.
pub fn facts(r: &Results) -> Facts {
    let mut f = Facts::default();
    // the biggest folder both smoothyfiles and File Explorer opened
    for (test, files) in [(OpenTest::Files100k, 100_000), (OpenTest::Files10k, 10_000), (OpenTest::Files1k, 1_000)] {
        let Some(o) = r.open.iter().find(|o| o.test == test) else { continue };
        let pick = |k: AppKind| o.apps.iter().filter(|a| a.app == k).filter_map(|a| a.median()).filter(|m| !m.timed_out).min_by(|a, b| a.settled_ms.total_cmp(&b.settled_ms));
        let (Some(ours), Some(ex)) = (pick(AppKind::Smoothyfiles), pick(AppKind::Explorer)) else { continue };
        f.open = ratio(ex.settled_ms, ours.settled_ms).map(|x| (files, x));
        // Explorer's memory only means something when its window had a process of its own
        // (otherwise it is the whole shell: taskbar, desktop…)
        // Task Manager's figure, measured while both showed the same folder (results saved
        // before it was measured say nothing about memory)
        if !ex.shared && !ours.shared && ex.private_ws > 0 && ours.private_ws > 0 {
            f.memory = ratio(ex.private_ws as f64, ours.private_ws as f64);
        }
        break;
    }
    f.copy = match (versus(&r.engines.copy_small), versus(&r.engines.copy_big)) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    };
    f.unzip = versus(&r.engines.unzip);
    f.search = r.engines.search_ms.filter(|_| r.engines.search_entries > 0).map(|ms| (ms, r.engines.search_entries));
    f
}

/// "1,6" / "1.6".
fn times(x: f64) -> String {
    let s = if x >= 10.0 { format!("{x:.0}") } else { format!("{x:.1}") };
    if crate::i18n::is_tr() { s.replace('.', ",") } else { s }
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let sep = if crate::i18n::is_tr() { '.' } else { ',' };
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(sep);
        }
        out.push(c);
    }
    out
}

/// Every message the strip can show, in order; `starts` = how many times smoothyfiles was opened.
pub fn messages(starts: u32) -> Vec<String> {
    let facts = FACTS.lock().unwrap_or_else(|e| e.into_inner()).clone().unwrap_or_default();
    messages_for(&facts, starts)
}

pub fn messages_for(f: &Facts, starts: u32) -> Vec<String> {
    let ask = t!("Geliştirmeye devam edebilmem için bir kahveyle destek olur musun?");
    let mut v = vec![t!("Merhaba, smoothyfiles'ı tek başıma, boş zamanlarımda geliştiriyorum. İşine yarıyorsa bana bir kahve ısmarlar mısın?").to_string()];
    if let Some((files, x)) = f.open {
        v.push(format!(
            "{} {ask}",
            t!("Bu bilgisayardaki hız testinde smoothyfiles {files} dosyalık bir klasörü Dosya Gezgini'nden {x} kat hızlı açtı.").replace("{files}", &thousands(files as u64)).replace("{x}", &times(x))
        ));
    }
    v.push(t!("smoothyfiles'ı şimdiye kadar {n} kez açtın; demek ki işine yarıyor. Bir kahveyle destek olursan bir sonraki özellik biraz daha çabuk gelir.").replace("{n}", &thousands(starts as u64)));
    if let Some(x) = f.memory {
        v.push(format!("{} {ask}", t!("Bu bilgisayardaki hız testinde smoothyfiles aynı klasörü gösterirken, Görev Yöneticisi'ne göre Dosya Gezgini'nden {x} kat daha az bellek kullandı.").replace("{x}", &times(x))));
    }
    v.push(t!("smoothyfiles ücretsiz, reklamsız ve açık kaynak; öyle de kalacak. Yaşaması, senin gibi kullananların küçük desteklerine bağlı.").to_string());
    if let Some(x) = f.copy {
        v.push(format!("{} {ask}", t!("Bu bilgisayardaki hız testinde smoothyfiles dosyaları Windows'tan {x} kat hızlı kopyaladı.").replace("{x}", &times(x))));
    }
    if let Some(x) = f.unzip {
        v.push(format!("{} {ask}", t!("Bu bilgisayardaki hız testinde smoothyfiles bir ZIP arşivini Windows'tan {x} kat hızlı çıkardı.").replace("{x}", &times(x))));
    }
    v.push(t!("Bu dosya yöneticisinin arkasında tek bir kişi ve epey kahve var. Bir fincanı da senden olursa çok sevinirim.").to_string());
    if let Some((ms, entries)) = f.search {
        v.push(format!(
            "{} {ask}",
            t!("Bu bilgisayardaki hız testinde smoothyfiles {entries} öğe arasında adla aramayı {ms} ms'de bitirdi.").replace("{entries}", &thousands(entries)).replace("{ms}", &format!("{ms:.0}"))
        ));
    }
    v.push(t!("Kullandığın için teşekkür ederim. Projeyi sürdürmeme yardım etmek istersen, küçük bir bağış bile çok şey ifade eder.").to_string());
    v
}

#[cfg(test)]
mod tests {
    use super::{Facts, ratio};

    #[test]
    fn only_real_differences_are_said() {
        assert_eq!(ratio(1228.0, 780.0).map(|x| (x * 10.0).round() / 10.0), Some(1.6));
        assert_eq!(ratio(162.0, 155.0), None); // 4 % less memory: not a claim
        assert_eq!(ratio(100.0, 0.0), None);
        let plain = super::messages_for(&Facts::default(), 12);
        let measured = super::messages_for(&Facts { open: Some((100_000, 1.57)), memory: Some(2.0), copy: Some(3.2), unzip: None, search: Some((40.0, 101_000)) }, 12);
        assert_eq!(plain.len(), 5);
        assert_eq!(measured.len(), 9);
        let (n, x) = (super::thousands(100_000), super::times(1.57));
        assert!(measured.iter().any(|m| m.contains(&n) && m.contains(&x)), "{measured:?}");
    }
}
