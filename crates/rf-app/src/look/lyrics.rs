//! Eşzamanlı şarkı sözü (LRC): yandaki aynı adlı `.lrc` dosyası.

/// LRC metninden (saniye, satır) listesi, zamana göre sıralı. `[offset:±ms]` uygulanır.
pub fn parse_lrc(text: &str) -> Vec<(f64, String)> {
    let mut offset = 0.0;
    let mut out = Vec::new();
    for raw in text.lines() {
        let mut rest = raw.trim();
        let mut stamps = Vec::new();
        while let Some(body) = rest.strip_prefix('[') {
            let Some(end) = body.find(']') else { break };
            let tag = &body[..end];
            rest = &body[end + 1..];
            if let Some(v) = tag.strip_prefix("offset:") {
                offset = v.trim().parse::<f64>().unwrap_or(0.0) / 1000.0;
                continue;
            }
            if let Some(t) = stamp(tag) {
                stamps.push(t);
            }
        }
        let line = rest.trim();
        for t in stamps {
            out.push((t, line.to_string()));
        }
    }
    for l in &mut out {
        l.0 = (l.0 - offset).max(0.0);
    }
    out.sort_by(|a, b| a.0.total_cmp(&b.0));
    out
}

/// "mm:ss.xx" ya da "mm:ss" → saniye.
fn stamp(tag: &str) -> Option<f64> {
    let (m, s) = tag.split_once(':')?;
    let m: f64 = m.trim().parse().ok()?;
    let s: f64 = s.trim().parse().ok()?;
    (m >= 0.0 && s >= 0.0).then_some(m * 60.0 + s)
}

/// `t` anında söylenen satır.
pub fn line_at(lines: &[(f64, String)], t: f64) -> Option<&str> {
    let i = lines.partition_point(|(s, _)| *s <= t);
    (i > 0).then(|| lines[i - 1].1.as_str())
}

/// Ses dosyasının yanındaki aynı adlı `.lrc` (kodlaması tanınarak).
pub fn sidecar(audio: &std::path::Path) -> Option<Vec<(f64, String)>> {
    let bytes = std::fs::read(audio.with_extension("lrc")).ok()?;
    let (text, _) = rf_core::textload::detect_and_decode(&bytes);
    let l = parse_lrc(&text);
    (!l.is_empty()).then_some(l)
}

#[cfg(test)]
mod tests {
    use super::{line_at, parse_lrc};

    #[test]
    fn parses_lrc() {
        let l = parse_lrc("[00:01.50]bir\n[00:03.00]iki\n[ar:x]");
        assert_eq!(l, vec![(1.5, "bir".into()), (3.0, "iki".into())]);
        assert_eq!(line_at(&l, 2.0), Some("bir"));
        assert_eq!(line_at(&l, 0.5), None);
    }

    #[test]
    fn multiple_stamps_and_offset() {
        let l = parse_lrc("[offset:+500]\n[00:02.00][00:10.00]nakarat\n[00:05]ara");
        // LRC: artı ofset sözleri erken gösterir
        assert_eq!(l, vec![(1.5, "nakarat".into()), (4.5, "ara".into()), (9.5, "nakarat".into())]);
    }
}
