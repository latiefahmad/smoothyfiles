//! Shared sidebar/home data: drives and pinned folders, loaded off the UI thread.

use std::path::PathBuf;

use gpui_kit::assets::IconName as L;
use gpui_kit::*;
use rf_win::{Drive, DriveKind, KnownFolder};

/// Drive display name with the generic fallback labels translated ("Yerel Disk (C:)").
pub fn drive_name(d: &Drive) -> String {
    if d.label.is_empty() {
        let kind = match d.kind {
            DriveKind::Removable => crate::t!("USB Sürücü"),
            DriveKind::Network => crate::t!("Ağ Sürücüsü"),
            DriveKind::Optical => crate::t!("DVD Sürücü"),
            _ => crate::t!("Yerel Disk"),
        };
        format!("{kind} ({}:)", d.letter)
    } else {
        d.display_name()
    }
}

#[derive(Clone)]
pub struct Pinned {
    pub title: String,
    pub path: PathBuf,
    pub icon: L,
}

#[derive(Clone)]
pub struct Recent {
    pub name: String,
    pub target: PathBuf,
    pub used_ms: i64,
    pub is_dir: bool,
}

/// Windows' own "recent items" (%APPDATA%\Microsoft\Windows\Recent\*.lnk), newest first.
fn load_recent(limit: usize) -> Vec<Recent> {
    let Some(dir) = dirs::data_dir().map(|d| d.join("Microsoft").join("Windows").join("Recent")) else {
        return Vec::new();
    };
    let Ok(rd) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut links: Vec<(std::time::SystemTime, PathBuf)> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("lnk")))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    links.sort_by(|a, b| b.0.cmp(&a.0));
    let mut out: Vec<Recent> = Vec::new();
    for (t, link) in links.into_iter().take(limit * 3) {
        let Ok(info) = rf_win::shortcut::resolve(&link) else { continue };
        if info.target.as_os_str().is_empty() || out.iter().any(|r| r.target == info.target) {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&info.target) else { continue };
        out.push(Recent {
            name: info.target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            target: info.target,
            used_ms: t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0),
            is_dir: meta.is_dir(),
        });
        if out.len() >= limit {
            break;
        }
    }
    out
}

/// Cloud folders (OneDrive, Dropbox, iCloud, Google Drive) and WSL distributions.
fn load_cloud_and_wsl() -> Vec<Pinned> {
    let mut out = Vec::new();
    let mut add = |title: String, path: PathBuf, icon: L| {
        if path.is_dir() && !out.iter().any(|p: &Pinned| p.path == path) {
            out.push(Pinned { title, path, icon });
        }
    };
    for (var, title) in [("OneDriveConsumer", "OneDrive"), ("OneDriveCommercial", "OneDrive (İş)"), ("OneDrive", "OneDrive")] {
        if let Some(v) = std::env::var_os(var) {
            add(title.into(), PathBuf::from(v), L::Cloud);
        }
    }
    if let Some(home) = dirs::home_dir() {
        add("iCloud Drive".into(), home.join("iCloudDrive"), L::Cloud);
        add("Dropbox".into(), home.join("Dropbox"), L::Cloud);
    }
    if let Some(local) = dirs::data_local_dir() {
        // Dropbox records its real location in info.json
        if let Ok(t) = std::fs::read_to_string(local.join("Dropbox").join("info.json")) {
            if let Some(i) = t.find("\"path\": \"") {
                let rest = &t[i + 9..];
                if let Some(j) = rest.find('"') {
                    add("Dropbox".into(), PathBuf::from(rest[..j].replace("\\\\", "\\")), L::Cloud);
                }
            }
        }
    }
    for letter in ['G', 'H'] {
        add("Google Drive".into(), PathBuf::from(format!("{letter}:\\My Drive")), L::Cloud);
    }
    // WSL distributions registered for this user
    if let Ok(lxss) = windows_registry::CURRENT_USER.open(r"Software\Microsoft\Windows\CurrentVersion\Lxss") {
        if let Ok(keys) = lxss.keys() {
            for k in keys {
                if let Ok(d) = lxss.open(&k) {
                    if let Ok(name) = d.get_string("DistributionName") {
                        let path = PathBuf::from(format!(r"\\wsl.localhost\{name}"));
                        out.push(Pinned { title: format!("WSL: {name}"), path, icon: L::SquareTerminal });
                    }
                }
            }
        }
    }
    out
}

pub struct Places {
    pub drives: Vec<Drive>,
    pub cloud: Vec<Pinned>,
    pub pinned: Vec<Pinned>,
    pub recent: Vec<Recent>,
    pub libraries: Vec<rf_win::libraries::Library>,
    /// Network locations ("Add a network location" shortcuts): (name, target).
    pub network: Vec<(String, std::path::PathBuf)>,
    /// Devices under This PC that are not drives (MTP phones, cameras): browsed as shell
    /// folders.
    pub devices: Vec<rf_win::shellfolder::ShellChild>,
    pub loaded: bool,
    _task: Option<Task<()>>,
}

impl Places {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut this =
            Self { drives: Vec::new(), cloud: Vec::new(), pinned: Vec::new(), recent: Vec::new(), libraries: Vec::new(), network: Vec::new(), devices: Vec::new(), loaded: false, _task: None };
        this.refresh(cx);
        // Notice drives appearing/disappearing (USB, network mappings) without any shell hooks.
        cx.spawn(async move |this, cx| {
            let mut mask = rf_win::drives::drive_mask();
            loop {
                cx.background_executor().timer(std::time::Duration::from_secs(3)).await;
                let now = rf_win::drives::drive_mask();
                if now != mask {
                    mask = now;
                    if this.update(cx, |p, cx| p.refresh(cx)).is_err() {
                        break;
                    }
                }
            }
        })
        .detach();
        // Phones come and go without a drive letter: ask the shell now and then. Off the UI
        // thread, and the list is only replaced when it changed. Not while no window is in front
        // of the user (minimised, or another app active): enumerating This PC loads shell
        // extensions and can touch the drives; the look happens again when one comes back.
        cx.spawn(async move |this, cx| loop {
            if !rf_win::window::app_in_front() {
                cx.background_executor().timer(std::time::Duration::from_secs(5)).await;
                if this.upgrade().is_none() {
                    break;
                }
                continue;
            }
            let found = cx.background_spawn(async move { rf_win::shellfolder::devices() }).await;
            let alive = this
                .update(cx, |p, cx| {
                    if p.devices != found {
                        p.devices = found;
                        cx.notify();
                    }
                })
                .is_ok();
            if !alive {
                break;
            }
            cx.background_executor().timer(std::time::Duration::from_secs(5)).await;
        })
        .detach();
        this
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let user_pins = cx.try_global::<crate::settings::Settings>().and_then(|s| s.pinned.clone());
        let work = cx.background_spawn(async move {
            let known = rf_win::known_folders();
            let pinned = match user_pins {
                None => known
                    .into_iter()
                    .map(|(k, path)| Pinned { title: k.title().to_string(), path, icon: known_icon(k) })
                    .collect::<Vec<_>>(),
                Some(paths) => paths
                    .into_iter()
                    .map(|path| match known.iter().find(|(_, p)| *p == path) {
                        Some((k, _)) => Pinned { title: k.title().to_string(), path, icon: known_icon(*k) },
                        None => Pinned {
                            title: rf_core::history::display_name(&path),
                            path,
                            icon: L::Folder,
                        },
                    })
                    .collect(),
            };
            let drives = rf_win::list_drives();
            let recent = load_recent(14);
            let cloud = load_cloud_and_wsl();
            let libraries = rf_win::libraries::list();
            let network = rf_win::network::network_shortcuts();
            (pinned, drives, recent, cloud, libraries, network)
        });
        self._task = Some(cx.spawn(async move |this, cx| {
            let (pinned, drives, recent, cloud, libraries, network) = work.await;
            let _ = this.update(cx, |p, cx| {
                p.pinned = pinned;
                p.drives = drives;
                p.recent = recent;
                p.cloud = cloud;
                p.libraries = libraries;
                p.network = network;
                p.loaded = true;
                cx.notify();
            });
        }));
    }
}

pub fn known_icon(k: KnownFolder) -> L {
    match k {
        KnownFolder::Desktop => L::Monitor,
        KnownFolder::Downloads => L::Download,
        KnownFolder::Documents => L::FileText,
        KnownFolder::Pictures => L::Image,
        KnownFolder::Music => L::Music,
        KnownFolder::Videos => L::Video,
        KnownFolder::Profile => L::House,
    }
}

pub fn drive_icon(d: &Drive) -> L {
    match d.kind {
        DriveKind::Removable => L::Usb,
        DriveKind::Network => L::Network,
        DriveKind::Optical => L::Disc3,
        _ => L::HardDrive,
    }
}
