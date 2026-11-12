//! Bilgi kartı: başka görüntüleyicinin açamadığı her şey (klasör, sürücü, bilinmeyen dosya, sanal
//! öğe, bulunamayan dosya). Kabuk küçük resmi, ad, değiştirme tarihi, boyut; klasörde arka planda
//! özyinelemeli sayım; sürücüde boş/toplam alan.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use gpui_kit::assets::IconName as L;
use gpui_kit::component::{Icon, h_flex, v_flex};
use gpui_kit::*;
use rf_core::{Entry, EntryKind, format};

use crate::image_cache::ImageCache;
use crate::look::viewer::{Prepared, Probe, Viewer};
use crate::t;
use crate::theme::Palette;

#[derive(Default)]
pub struct InfoCard {
    view: Option<Entity<InfoView>>,
}

impl Viewer for InfoCard {
    fn name(&self) -> &'static str {
        "info"
    }
    fn priority(&self) -> i32 {
        i32::MIN
    }
    fn can_handle(&self, _f: &Probe) -> bool {
        true
    }
    fn prepare(&mut self, f: &Probe, cx: &mut App) -> Prepared {
        let images = crate::look::LookManager::images(cx);
        let path = f.path.clone();
        let (exists, is_dir) = (f.exists, f.is_dir);
        self.view = Some(cx.new(|cx| InfoView::new(path, exists, is_dir, images, cx)));
        // (up to ten lines for a package, seven for an ELF file)
        let h = if f.exists && !f.is_dir && rf_win::apppkg::kind_of(&f.path).is_some() {
            380.0
        } else if f.exists && !f.is_dir && f.head.starts_with(b"ELF") {
            300.0
        } else if f.exists && !f.is_dir && (is_pe_ext(&f.path) || has_ext(&f.path, &["cer", "crt", "der", "pem"])) {
            260.0
        } else {
            210.0
        };
        let mut p = Prepared::new((520.0, h), super::file_name(&f.path));
        p.resizable = false;
        p
    }
    fn render(&mut self, _window: &mut Window, _cx: &mut App) -> AnyElement {
        match &self.view {
            Some(v) => v.clone().into_any_element(),
            None => div().into_any_element(),
        }
    }
    fn cleanup(&mut self) {
        self.view = None;
    }
}

pub struct InfoView {
    path: PathBuf,
    exists: bool,
    entry: Option<Entry>,
    images: Entity<ImageCache>,
    /// Klasör: (dosya, klasör, bayt, bitti mi).
    folder: Option<(u64, u64, u64, bool)>,
    /// Sürücü kökü: (boş, toplam, etiket).
    drive: Option<(u64, u64, String)>,
    /// Programs and libraries: version resource and architecture.
    pe: Option<rf_win::pe::PeInfo>,
    /// Certificate files: subject, issuer, validity.
    cert: Option<rf_win::cert::CertInfo>,
    /// App packages (MSIX, APK, IPA, NuGet): what the package says, and its icon.
    app: Option<(rf_win::apppkg::Kind, rf_win::apppkg::AppInfo)>,
    app_icon: Option<Arc<RenderImage>>,
    /// Linux / Android / embedded programs and libraries.
    elf: Option<rf_win::elf::ElfInfo>,
    cancel: Arc<AtomicBool>,
}

impl Drop for InfoView {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

fn ms(t: std::io::Result<std::time::SystemTime>) -> i64 {
    t.ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as i64)
}

fn has_ext(p: &Path, list: &[&str]) -> bool {
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| list.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

fn is_pe_ext(p: &Path) -> bool {
    const EXT: &[&str] = &["exe", "dll", "sys", "ocx", "cpl", "scr", "drv", "efi", "mui", "ax"];
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| EXT.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// Programs and libraries of Linux and the like: no extension, or one of theirs.
fn maybe_elf(p: &Path) -> bool {
    p.extension().is_none() || has_ext(p, &["so", "o", "ko", "elf", "axf", "out", "bin", "prx", "mod", "dbg"]) || p.to_string_lossy().contains(".so.")
}

fn is_drive_root(p: &Path) -> bool {
    let s = p.to_string_lossy();
    let b = s.as_bytes();
    (b.len() == 2 || (b.len() == 3 && (b[2] == b'\\' || b[2] == b'/'))) && b[1] == b':' && b[0].is_ascii_alphabetic()
}

impl InfoView {
    fn new(path: PathBuf, exists: bool, is_dir: bool, images: Entity<ImageCache>, cx: &mut Context<Self>) -> Self {
        cx.observe(&images, |_, _, cx| cx.notify()).detach();
        let entry = std::fs::metadata(&path).ok().map(|m| {
            let name = super::file_name(&path);
            let kind = if m.is_dir() { EntryKind::Dir } else { EntryKind::File };
            Entry::new(&name, kind, m.len(), ms(m.modified()), ms(m.created()), 0)
        });
        let mut v = Self { path, exists, entry, images, folder: None, drive: None, pe: None, cert: None, app: None, app_icon: None, elf: None, cancel: Default::default() };
        if is_dir && is_drive_root(&v.path) {
            let root = v.path.clone();
            let task = cx.background_spawn(async move { rf_win::volume::volume_info(&root).ok() });
            cx.spawn(async move |this, cx| {
                if let Some(i) = task.await {
                    let _ = this.update(cx, |v, cx| {
                        v.drive = Some((i.free, i.total, i.label));
                        cx.notify();
                    });
                }
            })
            .detach();
        } else if is_dir {
            v.count_folder(cx);
        } else if exists && has_ext(&v.path, &["cer", "crt", "der", "pem"]) {
            let file = v.path.clone();
            let task = cx.background_spawn(async move { rf_win::cert::info(&file) });
            cx.spawn(async move |this, cx| {
                if let Some(i) = task.await {
                    let _ = this.update(cx, |v, cx| {
                        v.cert = Some(i);
                        cx.notify();
                    });
                }
            })
            .detach();
        } else if exists && rf_win::apppkg::kind_of(&v.path).is_some() {
            let file = v.path.clone();
            let task = cx.background_spawn(async move {
                let (kind, mut info) = rf_win::apppkg::info(&file)?;
                let icon = info.icon_png.take().and_then(|b| rf_win::apppkg::icon(&b, 256));
                Some((kind, info, icon))
            });
            cx.spawn(async move |this, cx| {
                if let Some((kind, info, icon)) = task.await {
                    let _ = this.update(cx, |v, cx| {
                        v.app_icon = icon.as_ref().and_then(crate::image_cache::to_render_image);
                        v.app = Some((kind, info));
                        cx.notify();
                    });
                }
            })
            .detach();
        } else if exists && !is_dir && !is_pe_ext(&v.path) && maybe_elf(&v.path) {
            let file = v.path.clone();
            let task = cx.background_spawn(async move { rf_win::elf::is_elf(&file).then(|| rf_win::elf::info(&file)).flatten() });
            cx.spawn(async move |this, cx| {
                if let Some(i) = task.await {
                    let _ = this.update(cx, |v, cx| {
                        v.elf = Some(i);
                        cx.notify();
                    });
                }
            })
            .detach();
        } else if exists && is_pe_ext(&v.path) {
            let file = v.path.clone();
            let task = cx.background_spawn(async move { rf_win::pe::info(&file) });
            cx.spawn(async move |this, cx| {
                if let Some(i) = task.await {
                    let _ = this.update(cx, |v, cx| {
                        v.pe = Some(i);
                        cx.notify();
                    });
                }
            })
            .detach();
        }
        v
    }

    /// Klasörün içeriğini arka planda sayar; ara sonuçlar da gösterilir.
    fn count_folder(&mut self, cx: &mut Context<Self>) {
        self.folder = Some((0, 0, 0, false));
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<(u64, u64, u64, bool)>();
        let dir = self.path.clone();
        let cancel = self.cancel.clone();
        std::thread::Builder::new()
            .name("look-foldersize".into())
            .spawn(move || {
                let mut last = std::time::Instant::now();
                let tx2 = tx.clone();
                let r = crate::preview::size_cache().get_or_compute(&dir, &|| cancel.load(Ordering::Relaxed), &mut |s| {
                    if last.elapsed().as_millis() > 150 {
                        last = std::time::Instant::now();
                        let _ = tx2.unbounded_send((s.files, s.dirs, s.bytes, false));
                    }
                });
                if let Ok(s) = r {
                    let _ = tx.unbounded_send((s.files, s.dirs, s.bytes, true));
                }
            })
            .ok();
        cx.spawn(async move |this, cx| {
            use futures::StreamExt;
            while let Some(s) = rx.next().await {
                if this
                    .update(cx, |v, cx| {
                        v.folder = Some(s);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }
}

fn line(p: &Palette, k: &'static str, v: String) -> AnyElement {
    h_flex().gap_2().text_sm().child(div().text_color(p.text_secondary).child(k)).child(div().text_color(p.text).child(v)).into_any_element()
}

impl Render for InfoView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = Palette::for_window(window);
        let name = super::file_name(&self.path);
        let thumb = match (&self.entry, self.path.parent()) {
            _ if self.app_icon.is_some() => self.app_icon.clone(),
            (Some(e), Some(dir)) => {
                let (e, dir) = (e.clone(), dir.to_path_buf());
                self.images.update(cx, |c, cx| c.thumbnail(&dir, &e, 128, cx))
            }
            _ => None,
        };
        let mut lines: Vec<AnyElement> = Vec::new();
        if !self.exists && !self.path.to_string_lossy().starts_with("::") {
            lines.push(div().text_sm().text_color(p.danger).child(t!("Dosya bulunamadı")).into_any_element());
        }
        if let Some((free, total, label)) = &self.drive {
            if !label.is_empty() {
                lines.push(line(&p, t!("Etiket"), label.clone()));
            }
            lines.push(line(&p, t!("Boş"), format!("{} / {}", format::size(*free), format::size(*total))));
        } else if let Some((files, dirs, bytes, done)) = self.folder {
            let more = if done { String::new() } else { format!(" — {}", t!("hesaplanıyor…")) };
            lines.push(line(&p, t!("Boyut"), format!("{}{more}", format::size(bytes))));
            lines.push(line(&p, t!("İçerik"), format!("{files} {}, {dirs} {}", t!("dosya"), t!("klasör"))));
        } else if let Some(e) = self.entry.as_ref().filter(|e| !e.is_dir()) {
            lines.push(line(&p, t!("Boyut"), format::size(e.size)));
        }
        if let Some(c) = &self.cert {
            lines.push(line(&p, t!("Verilen"), c.subject.clone()));
            lines.push(line(&p, t!("Veren"), c.issuer.clone()));
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
            let state = if now > c.not_after {
                format!(" ({})", t!("süresi dolmuş"))
            } else if now < c.not_before {
                format!(" ({})", t!("henüz geçerli değil"))
            } else {
                String::new()
            };
            lines.push(line(&p, t!("Geçerlilik"), format!("{} – {}{state}", day(c.not_before), day(c.not_after))));
        }
        if let Some(pe) = &self.pe {
            if let Some(d) = pe.description.as_ref().or(pe.product.as_ref()) {
                lines.push(line(&p, t!("Açıklama"), d.clone()));
            }
            if let Some(c) = &pe.company {
                lines.push(line(&p, t!("Şirket"), c.clone()));
            }
            let ver = pe.version.clone().map_or_else(|| pe.arch.to_string(), |v| format!("{v} · {}", pe.arch));
            lines.push(line(&p, t!("Sürüm"), ver));
        }
        if let Some((kind, a)) = &self.app {
            let kind = match kind {
                rf_win::apppkg::Kind::Msix => t!("Windows uygulama paketi"),
                rf_win::apppkg::Kind::MsixBundle => t!("Windows uygulama paketi grubu"),
                rf_win::apppkg::Kind::Apk => t!("Android uygulaması"),
                rf_win::apppkg::Kind::Ipa => t!("iOS uygulaması"),
                rf_win::apppkg::Kind::Nuget => t!("NuGet paketi"),
            };
            lines.push(line(&p, t!("Tür"), kind.to_string()));
            if let Some(n) = &a.name {
                lines.push(line(&p, t!("Uygulama"), n.clone()));
            }
            if let Some(id) = &a.id {
                lines.push(line(&p, t!("Kimlik"), id.clone()));
            }
            if let Some(v) = &a.version {
                lines.push(line(&p, t!("Sürüm"), v.clone()));
            }
            if let Some(v) = &a.publisher {
                lines.push(line(&p, t!("Yayıncı"), v.clone()));
            }
            if !a.arch.is_empty() {
                lines.push(line(&p, t!("İşlemci"), a.arch.join(", ")));
            }
            if let Some(v) = &a.min_os {
                lines.push(line(&p, t!("En düşük sistem"), v.clone()));
            }
            if a.permissions > 0 {
                lines.push(line(&p, t!("İzinler"), a.permissions.to_string()));
            }
            if let Some(d) = &a.description {
                let short: String = d.chars().take(140).collect();
                lines.push(line(&p, t!("Açıklama"), if short.len() < d.len() { format!("{short}…") } else { short }));
            }
        }
        if let Some(e) = &self.elf {
            use rf_win::elf::Kind as K;
            let kind = match e.kind {
                K::Executable => t!("program"),
                K::Shared => t!("paylaşılan kitaplık"),
                K::Relocatable => t!("nesne dosyası"),
                K::Core => t!("bellek dökümü"),
                K::Unknown => "",
            };
            lines.push(line(&p, t!("Tür"), format!("ELF {} bit {kind}", e.bits).trim_end().to_string()));
            let os = e.os.map(|o| format!(" · {o}")).unwrap_or_default();
            lines.push(line(&p, t!("İşlemci"), format!("{}{os}", e.machine)));
            if let Some(i) = &e.interpreter {
                lines.push(line(&p, t!("Yükleyici"), i.clone()));
            }
            if let Some(s) = &e.soname {
                lines.push(line(&p, t!("Kitaplık adı"), s.clone()));
            }
            if !e.needed.is_empty() {
                let more = e.needed.len().saturating_sub(4);
                let mut s = e.needed.iter().take(4).cloned().collect::<Vec<_>>().join(", ");
                if more > 0 {
                    s.push_str(&format!(" +{more}"));
                }
                lines.push(line(&p, t!("Gerekli kitaplıklar"), s));
            }
        }
        if let Some(e) = &self.entry {
            if e.modified_ms > 0 {
                lines.push(line(&p, t!("Değiştirme"), format::datetime(e.modified_ms)));
            }
        }
        h_flex()
            .size_full()
            .p_5()
            .gap_5()
            .items_center()
            .child(
                div()
                    .size(px(128.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .map(|d| match thumb {
                        Some(img) => d.child(img_element(img)).into_any_element(),
                        None => {
                            let icon = if self.drive.is_some() || is_drive_root(&self.path) { L::HardDrive } else { L::File };
                            d.child(Icon::new(icon).size(px(72.)).text_color(p.text_secondary)).into_any_element()
                        }
                    }),
            )
            .child(
                v_flex()
                    .min_w_0()
                    .gap_1p5()
                    .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).text_color(p.text).truncate().child(name))
                    .children(lines),
            )
    }
}

fn img_element(img: Arc<RenderImage>) -> impl IntoElement {
    gpui_kit::img(img).max_w(px(128.)).max_h(px(128.)).object_fit(ObjectFit::Contain)
}

use gpui_kit::prelude::FluentBuilder as _;

/// Date only ("05.11.2026"), from the full local date-time.
fn day(ms: i64) -> String {
    format::datetime_full(ms).split(' ').next().unwrap_or_default().to_string()
}
