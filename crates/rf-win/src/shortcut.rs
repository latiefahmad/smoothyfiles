//! `.lnk` shortcuts via IShellLinkW + IPersistFile. Blocking + COM: call off the UI thread.

use std::path::{Path, PathBuf};

use windows::Win32::Storage::FileSystem::WIN32_FIND_DATAW;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, IPersistFile, STGM_READ};
use windows::Win32::UI::Shell::{IShellLinkDataList, IShellLinkW, ShellLink};
use windows::Win32::UI::WindowsAndMessaging::SHOW_WINDOW_CMD;
use windows::core::{Interface, PCWSTR};

use crate::shell::ensure_com;
use crate::wide::{from_wide, wide};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShortcutInfo {
    /// Empty when the target is not a file-system path (e.g. a Control Panel item).
    pub target: PathBuf,
    pub args: String,
    pub working_dir: PathBuf,
    /// Icon file ("" = use the target's icon) and its resource index.
    pub icon: String,
    pub icon_index: i32,
}

/// Reads a `.lnk` file.
pub fn resolve(path: &Path) -> anyhow::Result<ShortcutInfo> {
    ensure_com();
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        let file: IPersistFile = link.cast()?;
        let w = wide(path);
        file.Load(PCWSTR(w.as_ptr()), STGM_READ)?;

        let mut buf = vec![0u16; 32768];
        let mut fd = WIN32_FIND_DATAW::default();
        // SLGP_RAWPATH (0x4) keeps %env% variables unexpanded; we want the expanded one (flags 0).
        let _ = link.GetPath(&mut buf, &mut fd, 0);
        let target = from_wide(&buf);
        buf.fill(0);
        let _ = link.GetArguments(&mut buf);
        let args = from_wide(&buf);
        buf.fill(0);
        let _ = link.GetWorkingDirectory(&mut buf);
        let working_dir = from_wide(&buf);
        buf.fill(0);
        let mut icon_index = 0i32;
        let _ = link.GetIconLocation(&mut buf, &mut icon_index);
        let icon = from_wide(&buf);

        Ok(ShortcutInfo { target: target.into(), args, working_dir: working_dir.into(), icon, icon_index })
    }
}

/// Creates (or overwrites) `link_path` (should end in `.lnk`) pointing at `target`.
/// The working directory defaults to the target's folder (or the target itself for folders).
pub fn create(link_path: &Path, target: &Path) -> anyhow::Result<()> {
    ensure_com();
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        let t = wide(target);
        link.SetPath(PCWSTR(t.as_ptr()))?;
        let dir = if target.is_dir() { Some(target) } else { target.parent() };
        if let Some(d) = dir.filter(|d| !d.as_os_str().is_empty()) {
            let dw = wide(d);
            let _ = link.SetWorkingDirectory(PCWSTR(dw.as_ptr()));
        }
        let file: IPersistFile = link.cast()?;
        let l = wide(link_path);
        file.Save(PCWSTR(l.as_ptr()), true)?;
    }
    Ok(())
}

/// What the Shortcut tab shows and edits.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Details {
    /// The target as stored (environment variables unexpanded, SLGP_RAWPATH); "" when the link points at a non-file-system item.
    pub target: String,
    pub args: String,
    pub working_dir: String,
    /// The link's comment (IShellLinkW::GetDescription).
    pub comment: String,
    /// SW_SHOWNORMAL (1), SW_SHOWMAXIMIZED (3) or SW_SHOWMINNOACTIVE (7).
    pub show: i32,
    /// IShellLinkW::GetHotkey: low byte virtual key, high byte HOTKEYF_* modifiers (0 = none).
    pub hotkey: u16,
    pub icon: String,
    pub icon_index: i32,
    /// "Run as administrator" (SLDF_RUNAS_USER in IShellLinkDataList flags).
    pub run_as_admin: bool,
}

/// SLDF_RUNAS_USER: the link asks to run elevated.
const SLDF_RUNAS_USER: u32 = 0x2000;
/// SLGP_RAWPATH: give the path with %env% variables unexpanded.
const SLGP_RAWPATH: u32 = 0x4;

fn load_link(path: &Path) -> anyhow::Result<(IShellLinkW, IPersistFile)> {
    ensure_com();
    unsafe {
        let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        let file: IPersistFile = link.cast()?;
        let w = wide(path);
        file.Load(PCWSTR(w.as_ptr()), STGM_READ)?;
        Ok((link, file))
    }
}

/// Reads everything the Shortcut tab shows from a `.lnk` file.
pub fn details(path: &Path) -> anyhow::Result<Details> {
    let (link, _file) = load_link(path)?;
    unsafe {
        let mut buf = vec![0u16; 32768];
        let mut fd = WIN32_FIND_DATAW::default();
        let _ = link.GetPath(&mut buf, &mut fd, SLGP_RAWPATH);
        let target = from_wide(&buf);
        buf.fill(0);
        let _ = link.GetArguments(&mut buf);
        let args = from_wide(&buf);
        buf.fill(0);
        let _ = link.GetWorkingDirectory(&mut buf);
        let working_dir = from_wide(&buf);
        buf.fill(0);
        let _ = link.GetDescription(&mut buf);
        let comment = from_wide(&buf);
        buf.fill(0);
        let mut icon_index = 0i32;
        let _ = link.GetIconLocation(&mut buf, &mut icon_index);
        let icon = from_wide(&buf);
        let show = link.GetShowCmd().map(|s| s.0).unwrap_or(1);
        let hotkey = link.GetHotkey().unwrap_or(0);
        let run_as_admin = link.cast::<IShellLinkDataList>().and_then(|dl| dl.GetFlags()).is_ok_and(|f| f & SLDF_RUNAS_USER != 0);
        Ok(Details { target, args, working_dir, comment, show, hotkey, icon, icon_index, run_as_admin })
    }
}

/// Writes the editable parts back into the existing .lnk (IPersistFile::Save over the same file).
/// The target is only replaced when it changed and is not empty, so links to non-file-system items
/// (whose target is an ID list) keep it.
pub fn save(path: &Path, d: &Details) -> anyhow::Result<()> {
    let (link, file) = load_link(path)?;
    unsafe {
        let mut buf = vec![0u16; 32768];
        let mut fd = WIN32_FIND_DATAW::default();
        let _ = link.GetPath(&mut buf, &mut fd, SLGP_RAWPATH);
        if !d.target.is_empty() && from_wide(&buf) != d.target {
            link.SetPath(PCWSTR(wide(&d.target).as_ptr()))?;
        }
        link.SetArguments(PCWSTR(wide(&d.args).as_ptr()))?;
        link.SetWorkingDirectory(PCWSTR(wide(&d.working_dir).as_ptr()))?;
        link.SetDescription(PCWSTR(wide(&d.comment).as_ptr()))?;
        link.SetShowCmd(SHOW_WINDOW_CMD(d.show))?;
        link.SetHotkey(d.hotkey)?;
        link.SetIconLocation(PCWSTR(wide(&d.icon).as_ptr()), d.icon_index)?;
        if let Ok(dl) = link.cast::<IShellLinkDataList>() {
            let flags = dl.GetFlags()?;
            let want = if d.run_as_admin { flags | SLDF_RUNAS_USER } else { flags & !SLDF_RUNAS_USER };
            if want != flags {
                dl.SetFlags(want)?;
            }
        }
        let w = wide(path);
        file.Save(PCWSTR(w.as_ptr()), true)?;
    }
    Ok(())
}

/// "Ctrl + Alt + K" text for a hotkey as IShellLinkW stores it (empty for 0).
pub fn hotkey_text(hotkey: u16) -> String {
    let vk = (hotkey & 0xff) as u8;
    if vk == 0 {
        return String::new();
    }
    let mods = hotkey >> 8;
    let mut parts: Vec<String> = Vec::new();
    if mods & 0x02 != 0 {
        parts.push("Ctrl".into());
    }
    if mods & 0x01 != 0 {
        parts.push("Shift".into());
    }
    if mods & 0x04 != 0 {
        parts.push("Alt".into());
    }
    parts.push(match vk {
        b'0'..=b'9' | b'A'..=b'Z' => (vk as char).to_string(),
        0x70..=0x87 => format!("F{}", vk - 0x6f),
        0x60..=0x69 => format!("Num {}", vk - 0x60),
        0x6a => "Num *".into(),
        0x6b => "Num +".into(),
        0x6d => "Num -".into(),
        0x6e => "Num .".into(),
        0x6f => "Num /".into(),
        0x08 => "Backspace".into(),
        0x09 => "Tab".into(),
        0x0d => "Enter".into(),
        0x1b => "Esc".into(),
        0x20 => "Space".into(),
        0x21 => "Page Up".into(),
        0x22 => "Page Down".into(),
        0x23 => "End".into(),
        0x24 => "Home".into(),
        0x25 => "Left".into(),
        0x26 => "Up".into(),
        0x27 => "Right".into(),
        0x28 => "Down".into(),
        0x2d => "Insert".into(),
        0x2e => "Delete".into(),
        _ => format!("0x{vk:02X}"),
    });
    parts.join(" + ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_then_details() {
        let dir = std::env::temp_dir().join(format!("rf-lnk-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("hedef.txt");
        std::fs::write(&target, b"x").unwrap();
        let lnk = dir.join("hedef.lnk");
        create(&lnk, &target).unwrap();
        let mut d = details(&lnk).unwrap();
        assert!(d.target.ends_with("hedef.txt"), "{}", d.target);
        d.args = "--bayrak \"a b\"".into();
        d.comment = "deneme".into();
        d.show = 3;
        d.hotkey = 0x0200 | 0x0400 | b'K' as u16;
        d.run_as_admin = true;
        d.icon = r"C:\Windows\System32\shell32.dll".into();
        d.icon_index = 4;
        save(&lnk, &d).unwrap();
        let back = details(&lnk).unwrap();
        assert_eq!(back.args, d.args);
        assert_eq!(back.comment, "deneme");
        assert_eq!(back.show, 3);
        assert_eq!(back.hotkey, d.hotkey);
        assert!(back.run_as_admin);
        assert_eq!(back.icon.to_lowercase(), d.icon.to_lowercase());
        assert_eq!(back.icon_index, 4);
        assert_eq!(back.target, d.target);
        assert_eq!(hotkey_text(back.hotkey), "Ctrl + Alt + K");
        assert_eq!(hotkey_text(0), "");
        assert_eq!(hotkey_text(0x0100 | 0x70), "Shift + F1");
        d.run_as_admin = false;
        save(&lnk, &d).unwrap();
        assert!(!details(&lnk).unwrap().run_as_admin);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn create_then_resolve() {
        let dir = std::env::temp_dir().join(format!("rf-lnk-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("hedef.txt");
        std::fs::write(&target, b"x").unwrap();
        let lnk = dir.join("hedef.lnk");
        create(&lnk, &target).unwrap();
        assert!(lnk.exists());
        let info = resolve(&lnk).unwrap();
        // compare real paths: the temp folder may come as a short 8.3 name (C:\Users\RUNNER~1)
        // while the shortcut resolves to the long one
        let same = |a: &std::path::Path, b: &std::path::Path| std::fs::canonicalize(a).unwrap() == std::fs::canonicalize(b).unwrap();
        assert!(same(&info.target, &target), "{:?} vs {:?}", info.target, target);
        assert!(info.args.is_empty());
        assert!(same(&info.working_dir, &dir), "{:?} vs {:?}", info.working_dir, dir);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
