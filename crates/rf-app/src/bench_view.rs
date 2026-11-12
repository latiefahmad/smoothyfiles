//! Settings → Speed test: runs [`crate::bench`] on a background thread and shows the results:
//! a race played back at real speed (like latief.id/smoothyfiles), a table with time, CPU and memory per
//! app, our engines against Windows', then a star request and sharing.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName as L;
use gpui_kit::component::{Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use parking_lot::Mutex;
use rf_win::bench::{App as BenchApp, AppKind, Machine, OpenResult};

use crate::bench::{OpenTest, Progress, Results};
use crate::settings::{self, Settings};
use crate::t;
use crate::theme::{self, Palette};

const SITE: &str = "https://latief.id/smoothyfiles";
const REPO: &str = "https://github.com/latiefahmad/smoothyfiles";
/// One dot of the race track stands for this long.
const DOT_MS: f64 = 20.0;

/// What the background run reports.
#[derive(Default)]
struct Shared {
    progress: Option<Progress>,
    done: Option<Result<Results, String>>,
}

struct State {
    detected: Vec<BenchApp>,
    machine: Option<Machine>,
    off: HashSet<AppKind>,
    running: Option<(Arc<Mutex<Shared>>, Arc<AtomicBool>)>,
    progress: Option<Progress>,
    results: Option<Results>,
    error: Option<String>,
    race: OpenTest,
    replay: Instant,
    /// "Copied" feedback on the share buttons.
    note: Option<(String, Instant)>,
}
impl Global for State {}

fn state(cx: &mut App) -> &mut State {
    if !cx.has_global::<State>() {
        let detected = rf_win::bench::detect();
        cx.set_global(State {
            detected,
            machine: None,
            off: HashSet::new(),
            running: None,
            progress: None,
            results: crate::bench::last_results(),
            error: None,
            race: OpenTest::Files100k,
            replay: Instant::now() - Duration::from_secs(600),
            note: None,
        });
        // the machine line reads the registry and asks the disk: not on the UI thread
        cx.spawn(async move |cx| {
            let m = cx.background_spawn(async { rf_win::bench::machine(&crate::bench::bench_dir()) }).await;
            cx.update(|cx| {
                cx.global_mut::<State>().machine = Some(m);
                cx.refresh_windows();
            });
        })
        .detach();
    }
    cx.global_mut::<State>()
}

// ---------------------------------------------------------------- formatting

fn ms(v: f64) -> String {
    if v >= 10_000.0 {
        format!("{:.1} s", v / 1000.0)
    } else if v >= 1000.0 {
        format!("{:.2} s", v / 1000.0)
    } else {
        format!("{v:.0} ms")
    }
}

fn mb(bytes: u64) -> String {
    format!("{} MB", (bytes as f64 / 1_048_576.0).round() as u64)
}

/// 2330750 → "2.330.750" (Turkish) / "2,330,750".
pub(crate) fn count(n: u64) -> String {
    let sep = if crate::i18n::t("1.000 dosya").starts_with("1.000") { '.' } else { ',' };
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(sep);
        }
        out.push(c);
    }
    out
}

fn times(slow: f64, fast: f64) -> String {
    let x = slow / fast.max(1.0);
    if x >= 10.0 { format!("{x:.0}×") } else { format!("{x:.1}×") }
}

fn test_label(t: OpenTest) -> &'static str {
    match t {
        OpenTest::Start => t!("Açılış (boş klasör)"),
        OpenTest::Files1k => t!("1.000 dosya"),
        OpenTest::Files10k => t!("10.000 dosya"),
        OpenTest::Files100k => t!("100.000 dosya"),
        OpenTest::Images => t!("2.000 resim"),
    }
}

fn app_label(k: AppKind) -> &'static str {
    match k {
        AppKind::Smoothyfiles => "smoothyfiles",
        AppKind::Explorer => t!("Dosya Gezgini"),
        AppKind::Files => "Files",
        AppKind::FilePilot => "File Pilot",
    }
}

fn machine_line(m: &Machine) -> String {
    let mut parts = vec![m.cpu.clone()];
    if m.ram_gb > 0 {
        parts.push(format!("{} GB RAM", m.ram_gb));
    }
    if !m.disk.is_empty() {
        parts.push(m.disk.clone());
    }
    parts.push(m.windows.clone());
    parts.join(" · ")
}

/// smoothyfiles' median and the slowest other app's on `test`, for the headline.
fn speedup(r: &Results, test: OpenTest, other: AppKind) -> Option<(f64, f64)> {
    let o = r.open.iter().find(|o| o.test == test)?;
    let ours = o.apps.iter().find(|a| a.app == AppKind::Smoothyfiles)?.median()?.settled_ms;
    let theirs = o.apps.iter().find(|a| a.app == other)?.median()?.settled_ms;
    Some((ours, theirs))
}

// ---------------------------------------------------------------- running

fn start(cx: &mut App) {
    let st = state(cx);
    if st.running.is_some() {
        return;
    }
    let mut apps = vec![crate::bench::smoothyfiles_app()];
    apps.extend(st.detected.iter().filter(|a| !st.off.contains(&a.kind)).cloned());
    let plan = crate::bench::Plan { apps, open: OpenTest::ALL.to_vec(), copy: true, engines: true };
    let shared = Arc::new(Mutex::new(Shared::default()));
    let cancel = Arc::new(AtomicBool::new(false));
    st.running = Some((shared.clone(), cancel.clone()));
    st.error = None;
    st.progress = None;
    {
        let shared = shared.clone();
        std::thread::Builder::new()
            .name("rf-bench".into())
            .spawn(move || {
                let report = |p: Progress| shared.lock().progress = Some(p);
                let r = crate::bench::run(&plan, &report, &cancel).map_err(|e| format!("{e:#}"));
                shared.lock().done = Some(r);
                // the coffee strip quotes the newest results
                crate::donate::refresh();
            })
            .ok();
    }
    // mirror the thread's progress into the page until it finishes
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(Duration::from_millis(200)).await;
            let (p, done) = {
                let mut s = shared.lock();
                (s.progress.clone(), s.done.take())
            };
            let finished = cx.update(|cx| {
                let st = cx.global_mut::<State>();
                st.progress = p;
                if let Some(d) = done {
                    st.running = None;
                    match d {
                        Ok(r) => {
                            st.results = Some(r);
                            st.replay = Instant::now();
                        }
                        Err(e) if e == "cancelled" => {}
                        Err(e) => st.error = Some(e),
                    }
                    cx.refresh_windows();
                    return true;
                }
                cx.refresh_windows();
                false
            });
            if finished {
                break;
            }
        }
        replay_frames(cx).await;
    })
    .detach();
}

/// Redraws while the race replays.
async fn replay_frames(cx: &mut AsyncApp) {
    loop {
        let more = cx.update(|cx| {
            let st = cx.global::<State>();
            let longest = st.results.as_ref().map_or(0.0, |r| race_longest(r, st.race));
            let going = st.replay.elapsed().as_secs_f64() * 1000.0 < longest + 300.0;
            cx.refresh_windows();
            going
        });
        if !more {
            break;
        }
        cx.background_executor().timer(Duration::from_millis(33)).await;
    }
}

fn race_longest(r: &Results, test: OpenTest) -> f64 {
    r.open
        .iter()
        .find(|o| o.test == test)
        .map_or(0.0, |o| o.apps.iter().filter_map(|a| a.median()).map(|m| m.settled_ms).fold(0.0, f64::max))
}

fn replay(test: OpenTest, cx: &mut App) {
    let st = state(cx);
    st.race = test;
    st.replay = Instant::now();
    cx.spawn(async move |cx| replay_frames(cx).await).detach();
}

// ---------------------------------------------------------------- sharing

/// base64url without padding.
fn b64url(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for k in 0..c.len() + 1 {
            out.push(A[(n >> (18 - 6 * k) & 63) as usize] as char);
        }
    }
    out
}

fn app_key(k: AppKind) -> &'static str {
    match k {
        AppKind::Smoothyfiles => "rf",
        AppKind::Explorer => "ex",
        AppKind::Files => "fi",
        AppKind::FilePilot => "fp",
    }
}

/// The results as the site reads them (link format v1, agreed with the site): a small JSON,
/// base64url, in the path of latief.id/smoothyfiles/r/…. Measurements and the PC model only.
pub(crate) fn share_payload(r: &Results) -> serde_json::Value {
    use serde_json::{Map, Value, json};
    let mut open = Vec::new();
    let mut shared: Vec<&str> = Vec::new();
    for o in &r.open {
        let mut row = Map::new();
        let n = match o.test {
            OpenTest::Start => 0,
            OpenTest::Files1k => 1000,
            OpenTest::Files10k => 10000,
            OpenTest::Files100k => 100000,
            OpenTest::Images => {
                row.insert("img".into(), json!(true));
                2000
            }
        };
        row.insert("n".into(), json!(n));
        for a in &o.apps {
            let m = a.median();
            row.insert(app_key(a.app).into(), m.map_or(Value::Null, |m| json!(m.settled_ms.round() as i64)));
            if m.is_some_and(|m| m.shared) && !shared.contains(&app_key(a.app)) {
                shared.push(app_key(a.app));
            }
        }
        open.push(Value::Object(row));
    }
    let mut out = json!({
        "v": 1,
        "app": r.version,
        "t": r.when / 1000,
        "pc": {"cpu": r.machine.cpu, "ram": r.machine.ram_gb, "disk": r.machine.disk, "os": r.machine.windows, "cores": r.machine.cores},
        "open": open,
        "shared": shared,
    });
    let o = out.as_object_mut().expect("object");
    // CPU and memory from the largest folder that was opened
    let size = |t: OpenTest| match t {
        OpenTest::Files100k => 100000,
        OpenTest::Files10k => 10000,
        OpenTest::Files1k => 1000,
        _ => 0,
    };
    if let Some(l) = r.open.iter().filter(|x| size(x.test) > 0).max_by_key(|x| size(x.test)) {
        let (mut cpu, mut mem) = (Map::new(), Map::new());
        for a in &l.apps {
            let m = a.median();
            cpu.insert(app_key(a.app).into(), m.map_or(Value::Null, |m| json!(m.cpu_ms.round() as i64)));
            mem.insert(app_key(a.app).into(), m.map_or(Value::Null, |m| json!(((if m.private > 0 { m.private } else { m.ram }) as f64 / 1_048_576.0).round() as i64)));
        }
        o.insert("of".into(), json!(size(l.test)));
        o.insert("cpu".into(), Value::Object(cpu));
        o.insert("mem".into(), Value::Object(mem));
    }
    let e = &r.engines;
    let vs = |v: &Option<crate::bench::Versus>| v.as_ref().and_then(|v| Some((v.ours_ms?.round() as i64, v.windows_ms?.round() as i64)));
    // CPU time (ms) and extra memory (MB) of each side, when measured (0.5.1 on)
    let costs = |v: &crate::bench::Versus, m: &mut Map<String, Value>| {
        for (k, x) in [("rfc", v.ours_cpu_ms), ("winc", v.windows_cpu_ms), ("rfm", v.ours_mb), ("winm", v.windows_mb)] {
            if let Some(x) = x {
                m.insert(k.into(), json!(x.round() as i64));
            }
        }
    };
    for (key, v, extra) in [
        ("copy", &e.copy_small, json!({"n": 10000, "mb": 39})),
        ("big", &e.copy_big, json!({"mb": 1024})),
        ("zip", &e.unzip, json!({"n": 10000})),
    ] {
        if let (Some((a, b)), Some(full)) = (vs(v), v.as_ref()) {
            let Value::Object(mut m) = extra else { continue };
            m.insert("rf".into(), json!(a));
            m.insert("win".into(), json!(b));
            costs(full, &mut m);
            o.insert(key.into(), Value::Object(m));
        }
    }
    if let Some(ms) = e.search_ms {
        let mut m = Map::new();
        m.insert("n".into(), json!(e.search_entries));
        m.insert("rf".into(), json!(ms.round() as i64));
        if let Some(c) = e.search_cpu_ms {
            m.insert("c".into(), json!(c.round() as i64));
        }
        if let Some(x) = e.search_mb {
            m.insert("m".into(), json!(x.round() as i64));
        }
        o.insert("search".into(), Value::Object(m));
    }
    out
}

/// latief.id/smoothyfiles/r/… for these results.
pub(crate) fn share_link(r: &Results) -> String {
    let json = serde_json::to_vec(&share_payload(r)).unwrap_or_default();
    format!("{SITE}/r/{}", b64url(&json))
}

/// A short link from the site (latief.id/smoothyfiles/r/<8 characters>: the result is stored there), or
/// the long one carrying the result itself when the site can't be reached (offline, an error,
/// no answer in 5 s). Blocking: call it off the UI thread. Asked once per result.
pub(crate) fn short_link(r: &Results) -> String {
    static CACHE: std::sync::Mutex<Option<(i64, String)>> = std::sync::Mutex::new(None);
    if let Some((when, url)) = CACHE.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        if *when == r.when {
            return url.clone();
        }
    }
    let body = serde_json::to_vec(&share_payload(r)).unwrap_or_default();
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(std::time::Duration::from_secs(5))).http_status_as_error(false).build().into();
    let prefix = format!("{SITE}/r/");
    let got = agent
        .post(format!("{SITE}/api/r"))
        .header("User-Agent", format!("smoothyfiles/{}", crate::installer::version()))
        .content_type("application/json")
        .send(&body[..])
        .ok()
        .filter(|resp| resp.status() == 200)
        .and_then(|mut resp| resp.body_mut().read_to_string().ok())
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|v| v["url"].as_str().map(str::to_string))
        .filter(|url| url.starts_with(&prefix) && url.len() <= prefix.len() + 16);
    match got {
        Some(url) => {
            *CACHE.lock().unwrap_or_else(|e| e.into_inner()) = Some((r.when, url.clone()));
            url
        }
        None => share_link(r),
    }
}

/// Runs `f` with the results and their link (short when the site gives one), the request made
/// in the background so the window never waits on the network.
fn with_link(cx: &mut App, f: impl FnOnce(&Results, &str, &mut App) + 'static) {
    let Some(r) = state(cx).results.clone() else { return };
    let r2 = r.clone();
    cx.spawn(async move |cx| {
        let link = cx.background_executor().spawn(async move { short_link(&r2) }).await;
        let _ = cx.update(|cx| f(&r, &link, cx));
    })
    .detach();
}

fn summary_text(r: &Results, link: &str) -> String {
    let mut lines = vec![format!("smoothyfiles · {}", machine_line(&r.machine))];
    for o in &r.open {
        if !matches!(o.test, OpenTest::Files100k | OpenTest::Start) {
            continue;
        }
        // "*": the app was already running (Files keeps a process in the background), so the
        // time isn't a start from scratch: the card and the table say so, the text must too
        let row: Vec<String> = o
            .apps
            .iter()
            .filter_map(|a| a.median().map(|m| format!("{} {}{}", app_label(a.app), ms(m.settled_ms), if m.shared { "*" } else { "" })))
            .collect();
        if !row.is_empty() {
            lines.push(format!("{}: {}", test_label(o.test), row.join(", ")));
        }
    }
    let any_shared = r.open.iter().filter(|o| matches!(o.test, OpenTest::Files100k | OpenTest::Start)).any(|o| o.apps.iter().any(|a| a.median().is_some_and(|m| m.shared)));
    if let Some(v) = &r.engines.copy_small {
        if let (Some(a), Some(b)) = (v.ours_ms, v.windows_ms) {
            lines.push(format!("{}: smoothyfiles {} · Windows {}", t!("10.000 küçük dosya kopyalama"), ms(a), ms(b)));
        }
    }
    if any_shared {
        lines.push(format!("* {}", t!("uygulama zaten çalışıyordu (sıfırdan açılış değil)")));
    }
    lines.push(format!("{} {}", t!("Sonuçlar:"), link));
    lines.push(format!("{} {SITE}", t!("Ücretsiz ve açık kaynak:")));
    lines.join("\n")
}

/// The short line for X / WhatsApp / Reddit titles.
fn headline(r: &Results) -> String {
    match speedup(r, OpenTest::Files100k, AppKind::Explorer) {
        Some((ours, theirs)) => t!("smoothyfiles bilgisayarımda 100.000 dosyalık klasörü {ours} sürede açtı, Dosya Gezgini {theirs}.")
            .replace("{ours}", &ms(ours))
            .replace("{theirs}", &ms(theirs)),
        None => t!("smoothyfiles: Windows için hızlı, ücretsiz ve açık kaynak dosya yöneticisi.").to_string(),
    }
}

fn enc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn open_url(url: String) {
    std::thread::spawn(move || {
        let _ = rf_win::shell::open(std::path::Path::new(&url));
    });
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The result card as SVG (1200 × 675, the size social sites show whole).
fn card_svg(r: &Results) -> String {
    // 1200 × 675, the size social sites show whole; the same look as smoothyfiles' posts:
    // two-tone wordmark, one big multiplier, bars, three figures, a warm glow
    let (w, h) = (1200.0, 675.0);
    let orange = "#ff6a1a";
    let test = if r.open.iter().any(|o| o.test == OpenTest::Files100k) { OpenTest::Files100k } else { r.open.iter().map(|o| o.test).find(|t| *t != OpenTest::Start).unwrap_or(OpenTest::Files10k) };
    let row = r.open.iter().find(|o| o.test == test);
    let rows: Vec<(AppKind, f64, bool)> = row.map(|o| o.apps.iter().filter_map(|a| a.median().map(|m| (a.app, m.settled_ms, m.shared))).collect()).unwrap_or_default();
    let ours = rows.iter().find(|x| x.0 == AppKind::Smoothyfiles).map(|x| x.1);
    let explorer = rows.iter().find(|x| x.0 == AppKind::Explorer).map(|x| x.1);
    let mut s = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}">
<defs>
<radialGradient id="glow" cx="0.82" cy="0.42" r="0.55"><stop offset="0" stop-color="{orange}" stop-opacity="0.16"/><stop offset="1" stop-color="{orange}" stop-opacity="0"/></radialGradient>
<linearGradient id="hot" x1="0" y1="0" x2="1" y2="0"><stop offset="0" stop-color="{orange}"/><stop offset="1" stop-color="#ff8a45"/></linearGradient>
</defs>
<rect width="{w}" height="{h}" fill="#0f1013"/>
<rect width="{w}" height="{h}" fill="url(#glow)"/>
<text x="64" y="104" font-size="66" font-weight="800" letter-spacing="-2" fill="#f2f2f4">rusty<tspan fill="{orange}">files</tspan></text>
<text x="66" y="146" font-size="24" fill="#a3a6b0">{}</text>
<text x="66" y="178" font-size="18" fill="#6e717b">{}</text>
"##,
        esc(t!("Bilgisayarımdaki hız testi")),
        esc(&machine_line(&r.machine)),
    );
    // the headline: how much faster than File Explorer at opening the folder
    if let (Some(a), Some(b)) = (ours, explorer) {
        let headline = if b > a * 1.05 { times(b, a) } else { "=".into() };
        s.push_str(&format!(
            r##"<text x="62" y="330" font-size="132" font-weight="800" letter-spacing="-5" fill="{orange}">{}</text>
<text x="66" y="378" font-size="28" fill="#f2f2f4">{}</text>
<text x="66" y="410" font-size="20" fill="#6e717b">{}</text>
"##,
            esc(&headline),
            esc(if b > a * 1.05 { t!("Dosya Gezgini'nden hızlı") } else { t!("Dosya Gezgini kadar hızlı") }),
            esc(&t!("{test}: klasörün açılması").replace("{test}", test_label(test))),
        ));
    }
    // bars on the right
    let longest = rows.iter().map(|x| x.1).fold(1.0, f64::max);
    let (bx, bw) = (640.0, 420.0);
    s.push_str(&format!(r##"<text x="{bx}" y="236" font-size="18" fill="#6e717b">{}</text>"##, esc(t!("Klasörün açılması, gerçek hızda"))));
    for (i, (app, v, shared)) in rows.iter().enumerate() {
        let y = 262.0 + i as f64 * 62.0;
        let len = (v / longest * bw).max(10.0);
        let mine = *app == AppKind::Smoothyfiles;
        s.push_str(&format!(
            r##"<text x="{bx}" y="{}" font-size="20" fill="{}">{}</text><text x="{}" y="{}" font-size="20" font-weight="600" fill="{}" text-anchor="end">{}{}</text><rect x="{bx}" y="{}" width="{len:.0}" height="16" rx="5" fill="{}"/>"##,
            y + 18.0,
            if mine { "#f2f2f4" } else { "#a3a6b0" },
            esc(app_label(*app)),
            bx + bw + 76.0,
            y + 18.0,
            if mine { "#ff8a45" } else { "#f2f2f4" },
            esc(&ms(*v)),
            if *shared { " *" } else { "" },
            y + 28.0,
            if mine { "url(#hot)" } else { "#444955" },
        ));
    }
    // three figures
    let mut cells: Vec<(String, String)> = Vec::new();
    if let Some(v) = &r.engines.copy_small {
        if let (Some(a), Some(b)) = (v.ours_ms, v.windows_ms) {
            cells.push((times(b, a), t!("10.000 küçük dosya kopyalama").to_string()));
        }
    }
    if let Some(v) = &r.engines.unzip {
        if let (Some(a), Some(b)) = (v.ours_ms, v.windows_ms) {
            cells.push((times(b, a), t!("ZIP çıkarma: 10.000 dosya").to_string()));
        }
    }
    if let Some(m) = row.and_then(|o| o.apps.iter().find(|a| a.app == AppKind::Smoothyfiles)).and_then(|a| a.median()) {
        cells.push((ms(m.cpu_ms), t!("işlemci süresi").to_string()));
    }
    let n = cells.len().max(1) as f64;
    let (cx0, cw, cy) = (64.0, (w - 128.0) / n, 470.0);
    if !cells.is_empty() {
        s.push_str(&format!(r##"<rect x="{cx0}" y="{cy}" width="{}" height="104" rx="16" fill="#17191e" stroke="#262932"/>"##, w - 128.0));
    }
    for (i, (big, small)) in cells.iter().enumerate() {
        let x = cx0 + i as f64 * cw;
        if i > 0 {
            s.push_str(&format!(r##"<line x1="{x}" y1="{}" x2="{x}" y2="{}" stroke="#262932"/>"##, cy, cy + 104.0));
        }
        s.push_str(&format!(
            r##"<text x="{}" y="{}" font-size="44" font-weight="800" letter-spacing="-1" fill="#f2f2f4">{}</text><text x="{}" y="{}" font-size="18" fill="#a3a6b0">{}</text>"##,
            x + 28.0,
            cy + 56.0,
            esc(big),
            x + 30.0,
            cy + 86.0,
            esc(small)
        ));
    }
    s.push_str(&format!(
        r##"<text x="64" y="{}" font-size="22" font-weight="600" fill="#ff8a45">latief.id/smoothyfiles</text><text x="{}" y="{}" font-size="17" fill="#6e717b" text-anchor="end">{}</text></svg>"##,
        h - 40.0,
        w - 64.0,
        h - 40.0,
        esc(t!("Kendi bildirdiği sonuç · ücretsiz ve açık kaynak · Windows"))
    ));
    s
}

fn hwnd(window: &Window) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match HasWindowHandle::window_handle(window).map(|h| h.as_raw()) {
        Ok(RawWindowHandle::Win32(h)) => Some(h.hwnd.get()),
        _ => None,
    }
}

fn note(msg: &str, cx: &mut App) {
    state(cx).note = Some((msg.to_string(), Instant::now()));
    cx.spawn(async move |cx| {
        cx.background_executor().timer(Duration::from_millis(2600)).await;
        cx.update(|cx| cx.refresh_windows());
    })
    .detach();
}

fn copy_picture(window: &Window, cx: &mut App) {
    let Some(r) = state(cx).results.clone() else { return };
    let owner = hwnd(window);
    let svg = card_svg(&r);
    match rf_win::bench::render_card(&svg, 1.0) {
        Ok(pic) => {
            // also kept as a file next to the results, for attaching
            let path = crate::bench::bench_dir().join("smoothyfiles-speed-test.png");
            let _ = std::fs::write(&path, &pic.png);
            let ok = owner.is_some_and(|h| rf_win::clipboard::set_image(windows::Win32::Foundation::HWND(h as *mut _), pic.width, pic.height, &pic.rgba, &pic.png).is_ok());
            note(if ok { t!("Sonuç kartı panoya kopyalandı") } else { t!("Panoya kopyalanamadı") }, cx);
        }
        Err(e) => {
            log::warn!("bench card: {e:#}");
            note(t!("Kart çizilemedi"), cx);
        }
    }
}

/// Saves the card to Downloads and opens it.
fn save_picture(cx: &mut App) {
    let Some(r) = state(cx).results.clone() else { return };
    let svg = card_svg(&r);
    std::thread::spawn(move || {
        let Ok(pic) = rf_win::bench::render_card(&svg, 1.0) else { return };
        let dir = rf_win::known::KnownFolder::Downloads.path().unwrap_or_else(std::env::temp_dir);
        let path = rf_win::fileops::unique_name(&dir, "smoothyfiles speed test", ".png");
        if std::fs::write(&path, &pic.png).is_ok() {
            let _ = rf_win::shell::open(&path);
        }
    });
    note(t!("Kart İndirilenler klasörüne kaydedildi"), cx);
}

fn copy_text(window: &Window, cx: &mut App) {
    let _ = window;
    with_link(cx, |r, link, cx| {
        cx.write_to_clipboard(ClipboardItem::new_string(summary_text(r, link)));
        note(t!("Sonuçlar panoya kopyalandı"), cx);
    });
}

// ---------------------------------------------------------------- page

fn button(id: impl Into<ElementId>, p: &Palette, icon: L, label: impl Into<SharedString>, primary: bool, run: impl Fn(&mut Window, &mut App) + 'static) -> AnyElement {
    h_flex()
        .id(id)
        .h(px(32.))
        .px_3()
        .gap_2()
        .items_center()
        .rounded(theme::RADIUS_SM)
        .text_sm()
        .when(primary, |d| d.bg(p.accent).text_color(p.accent_text).hover(|s| s.opacity(0.9)))
        .when(!primary, |d| d.bg(p.hover).border_1().border_color(p.border).hover(|s| s.bg(p.pressed)))
        .active(|s| s.opacity(0.8))
        .child(Icon::new(icon).size(px(14.)))
        .child(label.into())
        .on_click(move |_, window, cx| run(window, cx))
        .into_any_element()
}

fn shell(p: &Palette) -> Div {
    v_flex().w_full().rounded(theme::RADIUS).bg(p.hover).border_1().border_color(p.border)
}

pub fn page(p: &Palette, cx: &mut App) -> Vec<AnyElement> {
    let st = state(cx);
    let running = st.running.is_some();
    let mut out = Vec::new();

    // intro: what it does, on which machine, with which apps
    let machine = st.machine.as_ref().map(machine_line).unwrap_or_default();
    let mut chips = h_flex().gap_2().flex_wrap().child(chip(p, "bench-app-rf", "smoothyfiles", true, true, |_| {}));
    let installed: Vec<AppKind> = st.detected.iter().map(|a| a.kind).collect();
    for kind in [AppKind::Explorer, AppKind::Files, AppKind::FilePilot] {
        let here = installed.contains(&kind);
        let on = here && !st.off.contains(&kind);
        let label = if here { app_label(kind).to_string() } else { format!("{} · {}", app_label(kind), t!("kurulu değil")) };
        chips = chips.child(chip(p, ("bench-app", kind as usize), &label, on, !here || running, move |cx| {
            let off = &mut state(cx).off;
            if !off.remove(&kind) {
                off.insert(kind);
            }
        }));
    }
    let action = if running {
        button("bench-stop", p, L::X, t!("Durdur"), false, |_, cx| {
            if let Some((_, c)) = &state(cx).running {
                c.store(true, Ordering::Relaxed);
            }
        })
    } else {
        button("bench-start", p, L::Play, if st.results.is_some() { t!("Yeniden çalıştır") } else { t!("Testi başlat") }, true, |_, cx| start(cx))
    };
    let mut intro = v_flex()
        .p_4()
        .gap_3()
        .child(
            h_flex()
                .gap_3()
                .items_center()
                .child(Icon::new(L::Gauge).size(px(22.)).text_color(p.accent))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().font_weight(FontWeight::SEMIBOLD).child(t!("Bu bilgisayarda hız testi")))
                        .child(div().text_xs().text_color(p.text_secondary).child(if machine.is_empty() { t!("Bilgisayar okunuyor…").to_string() } else { machine })),
                )
                .child(action),
        )
        .child(chips)
        .child(div().text_xs().text_color(p.text_secondary).child(t!(
            "Her uygulama aynı klasörleri sırayla açar; süre, işlemci ve bellek ölçülür. Test sırasında diğer uygulamaların pencereleri açılıp kapanır: fareye ve klavyeye dokunmayın. Yaklaşık 5–10 dakika sürer; test dosyaları ilk seferde hazırlanır (~1,3 GB, yerel uygulama verisinde)."
        )));
    if running {
        let (frac, label) = match &st.progress {
            Some(Progress::Step { i, n, label }) => (*i as f32 / (*n).max(1) as f32, format!("{} {i}/{n} · {label}", t!("Adım"))),
            Some(Progress::Preparing(what, f)) => (*f * 0.05, format!("{} {what}", t!("Test dosyaları hazırlanıyor:"))),
            None => (0.0, t!("Başlıyor…").to_string()),
        };
        intro = intro.child(
            v_flex()
                .gap_1p5()
                .child(div().h(px(4.)).w_full().rounded_full().bg(p.divider).child(div().h_full().w(relative(frac.clamp(0.01, 1.0))).rounded_full().bg(p.accent)))
                .child(div().text_xs().text_color(p.text_secondary).child(label)),
        );
    }
    if let Some(e) = &st.error {
        intro = intro.child(div().text_sm().text_color(p.danger).child(format!("{} {e}", t!("Test tamamlanamadı:"))));
    }
    out.push(shell(p).child(intro).into_any_element());

    let Some(r) = st.results.clone() else { return out };
    let race = st.race;
    let elapsed = st.replay.elapsed().as_secs_f64() * 1000.0;
    let note_text = st.note.as_ref().filter(|n| n.1.elapsed() < Duration::from_millis(2500)).map(|n| n.0.clone());
    out.push(race_card(p, &r, race, elapsed));
    out.push(table_card(p, &r));
    if let Some(e) = engines_card(p, &r) {
        out.push(e);
    }
    let star_asked = cx.global::<Settings>().star_asked;
    if !star_asked {
        out.push(star_card(p, &r));
    }
    out.push(share_card(p, note_text));
    out.push(
        h_flex()
            .pt_1()
            .gap_2()
            .text_xs()
            .text_color(p.text_tertiary)
            .child(div().flex_1().min_w_0().child(t!(
                "Ortanca değerler (3 çalıştırma, ısınma turu sayılmaz). Süre: uygulama başlatıldığından pencere içeriği 500 ms boyunca değişmeyene dek. * işaretli değerlerde uygulama zaten çalışıyordu: işlemci pencere açıldıktan sonrasını, bellek tüm süreci gösterir. Bellek, Görev Yöneticisi'nin gösterdiği değerdir."
            )))
            .child(
                div()
                    .id("bench-delete")
                    .cursor_pointer()
                    .text_color(p.accent)
                    .hover(|s| s.underline())
                    .child(t!("Test dosyalarını sil"))
                    .on_click(|_, _, cx| {
                        std::thread::spawn(|| {
                            let _ = crate::bench::delete_test_files();
                        });
                        state(cx).results = None;
                        cx.refresh_windows();
                    }),
            )
            .into_any_element(),
    );
    out
}

fn chip(p: &Palette, id: impl Into<ElementId>, label: &str, on: bool, locked: bool, toggle: impl Fn(&mut App) + 'static) -> AnyElement {
    h_flex()
        .id(id)
        .h(px(28.))
        .px_3()
        .gap_1p5()
        .items_center()
        .rounded_full()
        .border_1()
        .text_sm()
        .when(on, |d| d.bg(p.selection).border_color(p.accent))
        .when(!on, |d| d.border_color(p.border).text_color(p.text_secondary))
        .when(!locked, |d| d.hover(|s| s.bg(p.pressed)).on_click(move |_, _, cx| {
            toggle(cx);
            cx.refresh_windows();
        }))
        .when(on, |d| d.child(Icon::new(L::Check).size(px(12.)).text_color(p.accent)))
        .child(label.to_string())
        .into_any_element()
}

fn race_card(p: &Palette, r: &Results, race: OpenTest, elapsed: f64) -> AnyElement {
    let mut tabs = h_flex().gap_1();
    for (i, o) in r.open.iter().enumerate() {
        let t = o.test;
        let on = t == race;
        tabs = tabs.child(
            div()
                .id(("bench-tab", i))
                .px_3()
                .h(px(28.))
                .flex()
                .items_center()
                .rounded(theme::RADIUS_SM)
                .text_sm()
                .when(on, |d| d.bg(p.selection).text_color(p.text))
                .when(!on, |d| d.text_color(p.text_secondary).hover(|s| s.bg(p.pressed)))
                .child(test_label(t))
                .on_click(move |_, _, cx| replay(t, cx)),
        );
    }
    let o = r.open.iter().find(|o| o.test == race);
    let rows: Vec<(AppKind, Option<OpenResult>, Option<String>)> =
        o.map(|o| o.apps.iter().map(|a| (a.app, a.median().cloned(), a.error.clone())).collect()).unwrap_or_default();
    let longest = rows.iter().filter_map(|r| r.1.as_ref().map(|m| m.settled_ms)).fold(1.0, f64::max);
    let best = rows.iter().filter_map(|r| r.1.as_ref().map(|m| m.settled_ms)).fold(f64::MAX, f64::min);
    let mut lanes = v_flex().gap_3();
    for (app, m, err) in rows {
        let (track, value): (AnyElement, String) = match (&m, err) {
            (Some(m), _) => {
                let shown = elapsed.min(m.settled_ms);
                let done = elapsed >= m.settled_ms;
                let total_dots = (longest / DOT_MS).ceil().max(1.0);
                let lit = (shown / DOT_MS).ceil();
                let frac = (lit / total_dots).clamp(0.0, 1.0) as f32;
                let color = if app == AppKind::Smoothyfiles { p.accent } else { p.text_secondary };
                (
                    div().h(px(8.)).w_full().rounded_full().bg(p.divider).child(div().h_full().w(relative(frac.max(0.005))).rounded_full().bg(color)).into_any_element(),
                    if done { format!("{}{}", ms(m.settled_ms), if m.shared { " *" } else { "" }) } else { t!("Listeleniyor…").to_string() },
                )
            }
            (None, Some(_)) => (div().text_xs().text_color(p.text_tertiary).child(t!("ölçülemedi")).into_any_element(), "—".into()),
            _ => (div().into_any_element(), "—".into()),
        };
        let winner = m.as_ref().is_some_and(|m| m.settled_ms == best && elapsed >= m.settled_ms);
        lanes = lanes.child(
            h_flex()
                .gap_3()
                .items_center()
                .child(div().w(px(120.)).flex_shrink_0().text_sm().child(app_label(app)))
                .child(div().flex_1().min_w_0().child(track))
                .child(
                    div()
                        .w(px(96.))
                        .flex_shrink_0()
                        .text_sm()
                        .text_right()
                        .font_weight(if winner { FontWeight::SEMIBOLD } else { FontWeight::NORMAL })
                        .text_color(if winner { p.accent } else { p.text })
                        .child(value),
                ),
        );
    }
    shell(p)
        .p_4()
        .gap_4()
        .child(
            h_flex()
                .gap_3()
                .items_center()
                .child(div().flex_1().font_weight(FontWeight::SEMIBOLD).child(t!("Klasörün açılması, gerçek hızda")))
                .child(button("bench-replay", p, L::RotateCcw, t!("Tekrar oynat"), false, move |_, cx| replay(race, cx))),
        )
        .child(tabs)
        .child(lanes)
        .into_any_element()
}

fn table_card(p: &Palette, r: &Results) -> AnyElement {
    let apps: Vec<AppKind> = r.open.first().map(|o| o.apps.iter().map(|a| a.app).collect()).unwrap_or_default();
    let col = || div().flex_1().min_w(px(110.));
    let mut head = h_flex().px_4().py_2().gap_3().text_xs().text_color(p.text_secondary).child(div().w(px(150.)).flex_shrink_0().child(t!("Test")));
    for a in &apps {
        head = head.child(col().child(app_label(*a)));
    }
    let mut table = v_flex().child(head);
    for o in &r.open {
        let best = o.apps.iter().filter_map(|a| a.median().map(|m| m.settled_ms)).fold(f64::MAX, f64::min);
        let mut row = h_flex().px_4().py_2p5().gap_3().border_t_1().border_color(p.divider).items_start().child(div().w(px(150.)).flex_shrink_0().text_sm().child(test_label(o.test)));
        for a in &o.apps {
            let cell = match (a.median(), &a.error) {
                (Some(m), _) => {
                    let winner = m.settled_ms == best;
                    // Task Manager's figure; older results: private bytes, else the working set
                    let mem = if m.private_ws > 0 { mb(m.private_ws) } else if m.private > 0 { mb(m.private) } else { mb(m.ram) };
                    v_flex()
                        .gap_0p5()
                        .child(
                            div()
                                .text_sm()
                                .font_weight(if winner { FontWeight::SEMIBOLD } else { FontWeight::NORMAL })
                                .text_color(if winner { p.accent } else { p.text })
                                .child(format!("{}{}{}", ms(m.settled_ms), if m.shared { " *" } else { "" }, if m.timed_out { " ⏱" } else { "" })),
                        )
                        .child(div().text_xs().text_color(p.text_secondary).child(format!("{} {} · {} {}", t!("işlemci"), ms(m.cpu_ms), t!("bellek"), mem)))
                        .into_any_element()
                }
                (None, Some(e)) => div().text_xs().text_color(p.text_tertiary).child(format!("{} ({})", t!("ölçülemedi"), e.chars().take(60).collect::<String>())).into_any_element(),
                _ => div().child("—").into_any_element(),
            };
            row = row.child(col().child(cell));
        }
        table = table.child(row);
    }
    shell(p)
        .child(h_flex().px_4().pt_4().pb_2().child(div().font_weight(FontWeight::SEMIBOLD).child(t!("Ayrıntılar: süre, işlemci ve bellek"))))
        .child(table)
        .into_any_element()
}

fn engines_card(p: &Palette, r: &Results) -> Option<AnyElement> {
    let e = &r.engines;
    let mut rows: Vec<(String, String, String, String)> = Vec::new();
    // "işlemci süresi: smoothyfiles 1.20 s · Windows 3.40 s · bellek: +12 MB · +40 MB"
    let cost = |ours: (Option<f64>, Option<f64>), theirs: Option<(Option<f64>, Option<f64>, &str)>| -> String {
        let mb = |m: f64| format!("+{m:.0} MB");
        let mut parts = Vec::new();
        if let Some(c) = ours.0 {
            let mut s = format!("{}: smoothyfiles {}", t!("işlemci süresi"), ms(c));
            if let Some((Some(tc), _, name)) = theirs {
                s.push_str(&format!(" · {name} {}", ms(tc)));
            }
            parts.push(s);
        }
        if let Some(m) = ours.1 {
            let mut s = format!("{}: {}", t!("bellek"), mb(m));
            if let Some((_, Some(tm), _)) = theirs {
                s.push_str(&format!(" · {}", mb(tm)));
            }
            parts.push(s);
        }
        parts.join(" · ")
    };
    let mut versus = |label: &str, v: &Option<crate::bench::Versus>, windows_name: &str| {
        let Some(v) = v else { return };
        let usage = cost((v.ours_cpu_ms, v.ours_mb), Some((v.windows_cpu_ms, v.windows_mb, windows_name)));
        let (Some(a), Some(b)) = (v.ours_ms, v.windows_ms) else {
            if let Some(a) = v.ours_ms {
                rows.push((label.to_string(), format!("smoothyfiles {}", ms(a)), String::new(), usage));
            }
            return;
        };
        // within 10 %: the disk sets the pace for both
        let verdict = if (a - b).abs() <= a.max(b) * 0.1 {
            t!("aynı hızda").to_string()
        } else if b > a {
            t!("{x} daha hızlı").replace("{x}", &times(b, a))
        } else {
            t!("{x} daha yavaş").replace("{x}", &times(a, b))
        };
        rows.push((label.to_string(), format!("smoothyfiles {} · {windows_name} {}", ms(a), ms(b)), verdict, usage));
    };
    versus(t!("Kopyalama: 10.000 küçük dosya"), &e.copy_small, "Windows");
    versus(t!("Kopyalama: 1 GB'lık tek dosya"), &e.copy_big, "Windows");
    versus(t!("ZIP çıkarma: 10.000 dosya"), &e.unzip, "Windows");
    if let Some(s) = e.search_ms {
        let rate = e.search_entries as f64 / (s / 1000.0).max(0.001);
        let usage = cost((e.search_cpu_ms, e.search_mb), None);
        rows.push((t!("Alt klasörlerde arama").to_string(), format!("{} {} · {}", count(e.search_entries), t!("öğe"), ms(s)), t!("saniyede {n} öğe").replace("{n}", &count(rate as u64)), usage));
    }
    if rows.is_empty() {
        return None;
    }
    let mut list = v_flex();
    for (i, (label, detail, verdict, usage)) in rows.into_iter().enumerate() {
        list = list.child(
            h_flex()
                .px_4()
                .py_2p5()
                .gap_3()
                .items_center()
                .when(i > 0, |d| d.border_t_1().border_color(p.divider))
                .child(div().w(px(230.)).flex_shrink_0().text_sm().child(label))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_sm().text_color(p.text_secondary).child(detail))
                        // what each side cost the machine, under the times
                        .when(!usage.is_empty(), |d| d.child(div().text_xs().text_color(p.text_tertiary).child(usage))),
                )
                .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(p.accent).child(verdict)),
        );
    }
    Some(
        shell(p)
            .child(h_flex().px_4().pt_4().pb_2().child(div().font_weight(FontWeight::SEMIBOLD).child(t!("smoothyfiles motorları ve Windows"))))
            .child(list)
            .into_any_element(),
    )
}

fn star_card(p: &Palette, r: &Results) -> AnyElement {
    let line = match speedup(r, OpenTest::Files100k, AppKind::Explorer) {
        Some((ours, theirs)) if theirs > ours => {
            t!("smoothyfiles bu bilgisayarda büyük klasörü Dosya Gezgini'nden {x} hızlı açtı.").replace("{x}", &times(theirs, ours))
        }
        _ => t!("Test bitti.").to_string(),
    };
    shell(p)
        .p_4()
        .gap_3()
        .child(
            h_flex()
                .gap_3()
                .items_center()
                .child(Icon::new(L::Star).size(px(20.)).text_color(p.accent))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().font_weight(FontWeight::SEMIBOLD).child(line))
                        .child(div().text_sm().text_color(p.text_secondary).child(t!(
                            "smoothyfiles ücretsiz ve tek kişinin emeği. Beğendiysen GitHub'da bir yıldız ver: başkalarının da bulmasına yardım eder."
                        ))),
                )
                .child(button("bench-star", p, L::Star, t!("Yıldız ver"), true, |_, cx| {
                    open_url(REPO.into());
                    settings::update(cx, |s| s.star_asked = true);
                }))
                .child(button("bench-starred", p, L::Check, t!("Verdim"), false, |_, cx| settings::update(cx, |s| s.star_asked = true))),
        )
        .into_any_element()
}

fn share_card(p: &Palette, note_text: Option<String>) -> AnyElement {
    let social = |id: &'static str, label: &'static str, make: fn(&Results, &str) -> String| {
        button(id, p, L::Send, label, false, move |_, cx| with_link(cx, move |r, link, _| open_url(make(r, link))))
    };
    shell(p)
        .p_4()
        .gap_3()
        .child(
            h_flex()
                .gap_3()
                .items_center()
                .child(Icon::new(L::Share2).size(px(20.)).text_color(p.accent))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().font_weight(FontWeight::SEMIBOLD).child(t!("Arkadaşlarına öner")))
                        .child(div().text_sm().text_color(p.text_secondary).child(t!("Sonuç kartını paylaş: bilgisayarının özellikleri ve süreler tek bir görselde."))),
                )
                .children(note_text.map(|n| div().text_sm().text_color(p.accent).child(n))),
        )
        .child(
            h_flex()
                .gap_2()
                .flex_wrap()
                .child(button("bench-copy-card", p, L::Image, t!("Kartı kopyala"), true, |w, cx| copy_picture(w, cx)))
                .child(button("bench-save-card", p, L::Download, t!("Kartı kaydet"), false, |_, cx| save_picture(cx)))
                .child(button("bench-copy-text", p, L::Copy, t!("Metni kopyala"), false, |w, cx| copy_text(w, cx)))
                .child(button("bench-copy-link", p, L::Link, t!("Bağlantıyı kopyala"), false, |_, cx| {
                    with_link(cx, |_, link, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(link.to_string()));
                        note(t!("Bağlantı kopyalandı"), cx);
                    })
                }))
                .child(social("bench-x", "X", |r, l| format!("https://twitter.com/intent/tweet?text={}&url={}", enc(&headline(r)), enc(l))))
                .child(social("bench-wa", "WhatsApp", |r, l| format!("https://wa.me/?text={}", enc(&format!("{} {}", headline(r), l)))))
                .child(social("bench-reddit", "Reddit", |r, l| format!("https://www.reddit.com/submit?url={}&title={}", enc(l), enc(&headline(r)))))
                .child(social("bench-li", "LinkedIn", |_, l| format!("https://www.linkedin.com/sharing/share-offsite/?url={}", enc(l)))),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    /// Draws the card from the last results on this machine (when there are any) to %TEMP%.
    #[test]
    fn card_renders() {
        let Some(r) = crate::bench::last_results() else { return };
        let pic = rf_win::bench::render_card(&super::card_svg(&r), 1.0).unwrap();
        assert_eq!((pic.width, pic.height), (1200, 675));
        std::fs::write(std::env::temp_dir().join("rf-bench-card.png"), &pic.png).unwrap();
    }

    #[test]
    fn base64url_matches_the_standard() {
        // RFC 4648 test vectors, url alphabet, no padding
        for (i, o) in [("", ""), ("f", "Zg"), ("fo", "Zm8"), ("foo", "Zm9v"), ("foob", "Zm9vYg"), ("fooba", "Zm9vYmE"), ("foobar", "Zm9vYmFy")] {
            assert_eq!(super::b64url(i.as_bytes()), o);
        }
        assert_eq!(super::b64url(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn payload_follows_the_site_format() {
        let Some(r) = crate::bench::last_results() else { return };
        let v = super::share_payload(&r);
        assert_eq!(v["v"], 1);
        assert!(v["pc"]["cpu"].is_string() && v["open"].is_array());
        let link = super::share_link(&r);
        assert!(link.starts_with("https://latief.id/smoothyfiles/r/") && link.len() < 2900, "{link}");
    }

    #[test]
    fn counts_get_separators() {
        let c = super::count(2_330_750);
        assert!(c == "2.330.750" || c == "2,330,750", "{c}");
        assert_eq!(super::count(999), "999");
    }
}
