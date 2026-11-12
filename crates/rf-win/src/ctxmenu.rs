//! Native Explorer context menu (IContextMenu), shown from the shell STA thread.

use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    CMF_CANRENAME, CMF_EXPLORE, CMF_EXTENDEDVERBS, CMF_NORMAL, CMIC_MASK_PTINVOKE,
    CMINVOKECOMMANDINFO, CMINVOKECOMMANDINFOEX, GCS_VERBW, IContextMenu, IContextMenu2, IContextMenu3, ILFindLastID,
    IShellFolder, SHBindToObject, SHParseDisplayName,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, DestroyMenu, InsertMenuW, MF_BYPOSITION, MF_GRAYED, MF_SEPARATOR, MF_STRING, PostMessageW, SW_SHOWNORMAL, SetForegroundWindow, TPM_RETURNCMD, TPM_RIGHTBUTTON,
    TrackPopupMenuEx, WM_DRAWITEM, WM_INITMENUPOPUP, WM_MEASUREITEM, WM_MENUCHAR, WM_NULL,
};
use windows::core::{Interface, PCSTR, PCWSTR, PSTR};

use crate::sta::MENU_HOOK;

const CMIC_MASK_UNICODE: u32 = 0x4000;
use crate::wide::wide;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuOutcome {
    Dismissed,
    /// User chose "Rename": the app should start inline rename.
    Rename,
    /// A shell verb ran (e.g. "delete", "copy", "properties", or a handler without a verb).
    Invoked(String),
    /// One of the app's own items (see `ExtraItem`) was chosen.
    Custom(u32),
}

/// App-provided entries shown above the shell's items. `id` is returned in `MenuOutcome::Custom`.
#[derive(Debug, Clone)]
pub struct ExtraItem {
    pub id: u32,
    pub label: String,
    pub enabled: bool,
}

const EXTRA_BASE: u32 = 0x9000;

struct Pidl(*mut ITEMIDLIST);
impl Drop for Pidl {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CoTaskMemFree(Some(self.0 as *const _)) }
        }
    }
}

fn parse(path: &Path) -> anyhow::Result<Pidl> {
    let w = wide(path);
    let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
    unsafe { SHParseDisplayName(PCWSTR(w.as_ptr()), None, &mut pidl, 0, None)? };
    Ok(Pidl(pidl))
}

fn folder(path: &Path) -> anyhow::Result<IShellFolder> {
    let pidl = parse(path)?;
    Ok(unsafe { SHBindToObject::<_, _, IShellFolder>(None, pidl.0, None)? })
}

fn menu_for_items(owner: HWND, items: &[PathBuf]) -> anyhow::Result<IContextMenu> {
    anyhow::ensure!(!items.is_empty(), "no items");
    let parent = items[0].parent().ok_or_else(|| anyhow::anyhow!("no parent"))?;
    // Child pidls are resolved relative to ONE folder: items from other folders (search/tag
    // results) would silently map to the wrong files, so leave them out.
    let items: Vec<&PathBuf> = items.iter().filter(|p| p.parent() == Some(parent)).collect();
    let sf = folder(parent)?;
    let abs: Vec<Pidl> = items.iter().map(|p| parse(p)).collect::<anyhow::Result<_>>()?;
    let children: Vec<*const ITEMIDLIST> = abs.iter().map(|p| unsafe { ILFindLastID(p.0) as *const _ }).collect();
    Ok(unsafe { sf.GetUIObjectOf(owner, &children, None)? })
}

/// Bound through the item's real parent in the shell namespace, so drive roots (`C:\`,
/// parent = "This PC") work too.
fn menu_for_one(owner: HWND, item: &Path) -> anyhow::Result<IContextMenu> {
    let abs = parse(item)?;
    let mut last: *mut ITEMIDLIST = std::ptr::null_mut();
    let sf: IShellFolder = unsafe { windows::Win32::UI::Shell::SHBindToParent(abs.0, Some(&mut last))? };
    let children = [last as *const ITEMIDLIST];
    Ok(unsafe { sf.GetUIObjectOf(owner, &children, None)? })
}

fn menu_for_background(owner: HWND, dir: &Path) -> anyhow::Result<IContextMenu> {
    let sf = folder(dir)?;
    Ok(unsafe { sf.CreateViewObject(owner)? })
}

/// Must be called on the shell STA thread (see `sta::run`) with its owner window.
pub fn show_for_items(
    owner: HWND,
    items: &[PathBuf],
    screen: (i32, i32),
    extended: bool,
    extra: &[ExtraItem],
) -> anyhow::Result<MenuOutcome> {
    let cm = menu_for_items(owner, items)?;
    run_menu(owner, &cm, screen, extended, true, extra)
}

/// Menu for one item (used by the sidebar).
pub fn show_for_one(
    owner: HWND,
    item: &Path,
    screen: (i32, i32),
    extended: bool,
    extra: &[ExtraItem],
) -> anyhow::Result<MenuOutcome> {
    let cm = menu_for_one(owner, item)?;
    // no inline rename from the sidebar
    run_menu(owner, &cm, screen, extended, false, extra)
}

/// Background (empty space) menu of a folder.
pub fn show_for_background(
    owner: HWND,
    dir: &Path,
    screen: (i32, i32),
    extended: bool,
    extra: &[ExtraItem],
) -> anyhow::Result<MenuOutcome> {
    let cm = menu_for_background(owner, dir)?;
    run_menu(owner, &cm, screen, extended, false, extra)
}

// ---------- the shell menu as data (rendered by the app's own Fluent menu) ----------

/// What a shell menu is built for.
#[derive(Debug, Clone)]
pub enum ShellTarget {
    /// Items of one folder (the selection).
    Items(Vec<PathBuf>),
    /// A single item through its real parent (sidebar entries, drive roots).
    One(PathBuf),
    /// The empty area of a folder.
    Background(PathBuf),
}

/// A menu bitmap as top-down BGRA rows (premultiplied, as the shell hands them out).
#[derive(Debug, Clone)]
pub struct ShellIcon {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct ShellItem {
    /// Command id for [`invoke`]; 0 for separators and submenus.
    pub id: u32,
    pub label: String,
    /// Text after a tab in the label (shortcut hint), if any.
    pub hint: Option<String>,
    /// Canonical verb ("open", "properties", "Windows.ModernShare"…), empty if the handler has none.
    pub verb: String,
    pub icon: Option<ShellIcon>,
    pub enabled: bool,
    pub separator: bool,
    pub children: Vec<ShellItem>,
}

struct Session {
    id: u64,
    cm: IContextMenu,
    hmenu: windows::Win32::UI::WindowsAndMessaging::HMENU,
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyMenu(self.hmenu);
        }
    }
}

thread_local! {
    /// The one live shell menu (kept until invoked, released, or replaced by the next one).
    static SESSION: std::cell::RefCell<Option<Session>> = const { std::cell::RefCell::new(None) };
}
static NEXT_SESSION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn context_menu_for(owner: HWND, target: &ShellTarget) -> anyhow::Result<(IContextMenu, bool)> {
    Ok(match target {
        ShellTarget::Items(items) => (menu_for_items(owner, items)?, true),
        ShellTarget::One(item) => (menu_for_one(owner, item)?, false),
        ShellTarget::Background(dir) => (menu_for_background(owner, dir)?, false),
    })
}

/// Builds the shell menu for `target` and returns it as data plus a session id for
/// [`invoke`]. Submenus that fill themselves on demand (Send to, Open with…) are initialised
/// here. Call on the shell STA thread.
pub fn build(owner: HWND, target: &ShellTarget, extended: bool) -> anyhow::Result<(u64, Vec<ShellItem>)> {
    let (cm, can_rename) = context_menu_for(owner, target)?;
    unsafe {
        let hmenu = CreatePopupMenu()?;
        let mut flags = CMF_NORMAL | CMF_EXPLORE;
        if can_rename {
            flags |= CMF_CANRENAME;
        }
        if extended {
            flags |= CMF_EXTENDEDVERBS;
        }
        if let Err(e) = cm.QueryContextMenu(hmenu, 0, 1, 0x7FFF, flags).ok() {
            let _ = DestroyMenu(hmenu);
            return Err(e.into());
        }
        let cm3: Option<IContextMenu3> = cm.cast().ok();
        let cm2: Option<IContextMenu2> = cm.cast().ok();
        let items = read_menu(&cm, cm3.as_ref(), cm2.as_ref(), hmenu, 0);
        let id = NEXT_SESSION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        SESSION.with(|s| *s.borrow_mut() = Some(Session { id, cm, hmenu }));
        Ok((id, items))
    }
}

/// Runs command `id` of session `session` (from [`build`]). `screen` is where the menu was
/// opened (some handlers place their UI there). Call on the shell STA thread.
pub fn invoke(owner: HWND, session: u64, id: u32, screen: (i32, i32)) -> anyhow::Result<MenuOutcome> {
    let s = SESSION.with(|s| s.borrow_mut().take());
    let Some(s) = s.filter(|s| s.id == session) else {
        anyhow::bail!("menu session expired");
    };
    let r = invoke_offset(owner, &s.cm, id - 1, screen);
    drop(s);
    r
}

/// Drops the live session if it is `session` (menu closed without choosing a shell command).
pub fn release(session: u64) {
    SESSION.with(|s| {
        let mut s = s.borrow_mut();
        if s.as_ref().is_some_and(|x| x.id == session) {
            *s = None;
        }
    });
}

/// Invokes a canonical verb ("properties", "Windows.ModernShare", "openas"…) on `target`
/// without showing a menu. Call on the shell STA thread.
pub fn invoke_verb(owner: HWND, target: &ShellTarget, verb: &str, screen: (i32, i32)) -> anyhow::Result<()> {
    let (cm, _) = context_menu_for(owner, target)?;
    unsafe {
        // handlers only answer verbs after QueryContextMenu
        let hmenu = CreatePopupMenu()?;
        let _ = cm.QueryContextMenu(hmenu, 0, 1, 0x7FFF, CMF_NORMAL | CMF_EXPLORE);
        let verb_a = std::ffi::CString::new(verb).unwrap_or_default();
        let verb_w = wide(verb);
        let info = CMINVOKECOMMANDINFOEX {
            cbSize: std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32,
            fMask: CMIC_MASK_UNICODE | CMIC_MASK_PTINVOKE,
            hwnd: owner,
            lpVerb: PCSTR(verb_a.as_ptr() as *const u8),
            lpVerbW: PCWSTR(verb_w.as_ptr()),
            nShow: SW_SHOWNORMAL.0,
            ptInvoke: POINT { x: screen.0, y: screen.1 },
            ..Default::default()
        };
        let r = cm.InvokeCommand(&info as *const _ as *const CMINVOKECOMMANDINFO);
        let _ = DestroyMenu(hmenu);
        r.map_err(Into::into)
    }
}

fn verb_of(cm: &IContextMenu, offset: usize) -> String {
    let mut buf = [0u16; 256];
    unsafe {
        cm.GetCommandString(offset, GCS_VERBW, None, PSTR(buf.as_mut_ptr() as *mut u8), buf.len() as u32)
            .ok()
            .map(|_| crate::wide::from_wide(&buf))
            .unwrap_or_default()
    }
}

fn invoke_offset(owner: HWND, cm: &IContextMenu, offset: u32, (x, y): (i32, i32)) -> anyhow::Result<MenuOutcome> {
    let verb = verb_of(cm, offset as usize);
    if verb.eq_ignore_ascii_case("rename") {
        return Ok(MenuOutcome::Rename);
    }
    let info = CMINVOKECOMMANDINFOEX {
        cbSize: std::mem::size_of::<CMINVOKECOMMANDINFOEX>() as u32,
        fMask: CMIC_MASK_UNICODE | CMIC_MASK_PTINVOKE,
        hwnd: owner,
        lpVerb: PCSTR(offset as usize as *const u8),
        lpVerbW: PCWSTR(offset as usize as *const u16),
        nShow: SW_SHOWNORMAL.0,
        ptInvoke: POINT { x, y },
        ..Default::default()
    };
    unsafe { cm.InvokeCommand(&info as *const _ as *const CMINVOKECOMMANDINFO) }
        .map(|_| MenuOutcome::Invoked(verb))
        .map_err(Into::into)
}

/// "&Open\tCtrl+O" → ("Open", Some("Ctrl+O")); "&&" stays a literal ampersand.
pub fn clean_label(raw: &str) -> (String, Option<String>) {
    let (text, hint) = match raw.split_once('\t') {
        Some((t, h)) => (t, Some(h.trim().to_string()).filter(|h| !h.is_empty())),
        None => (raw, None),
    };
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '&' {
            if chars.peek() == Some(&'&') {
                chars.next();
                out.push('&');
            }
            continue;
        }
        out.push(c);
    }
    (out.trim().to_string(), hint)
}

unsafe fn read_menu(
    cm: &IContextMenu,
    cm3: Option<&IContextMenu3>,
    cm2: Option<&IContextMenu2>,
    hmenu: windows::Win32::UI::WindowsAndMessaging::HMENU,
    depth: u32,
) -> Vec<ShellItem> {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetMenuItemCount, GetMenuItemInfoW, MENUITEMINFOW, MFS_DISABLED, MFT_OWNERDRAW, MFT_SEPARATOR, MIIM_BITMAP,
        MIIM_FTYPE, MIIM_ID, MIIM_STATE, MIIM_STRING, MIIM_SUBMENU,
    };
    use windows::core::PWSTR;
    let mut out = Vec::new();
    let n = unsafe { GetMenuItemCount(Some(hmenu)) };
    for i in 0..n.max(0) {
        let mut buf = [0u16; 512];
        let mut mii = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_FTYPE | MIIM_STATE | MIIM_ID | MIIM_SUBMENU | MIIM_STRING | MIIM_BITMAP,
            dwTypeData: PWSTR(buf.as_mut_ptr()),
            cch: (buf.len() - 1) as u32,
            ..Default::default()
        };
        if unsafe { GetMenuItemInfoW(hmenu, i as u32, true, &mut mii) }.is_err() {
            continue;
        }
        if mii.fType.contains(MFT_SEPARATOR) {
            out.push(ShellItem {
                id: 0,
                label: String::new(),
                hint: None,
                verb: String::new(),
                icon: None,
                enabled: false,
                separator: true,
                children: Vec::new(),
            });
            continue;
        }
        let raw = String::from_utf16_lossy(&buf[..(mii.cch as usize).min(buf.len())]);
        let (label, hint) = clean_label(&raw);
        if label.is_empty() || mii.fType.contains(MFT_OWNERDRAW) && label.is_empty() {
            continue;
        }
        let mut children = Vec::new();
        if !mii.hSubMenu.is_invalid() {
            if depth >= 3 {
                continue;
            }
            // let lazily filled submenus (Send to, Open with…) populate themselves
            let (wp, lp) = (WPARAM(mii.hSubMenu.0 as usize), LPARAM(i as isize));
            unsafe {
                if let Some(c3) = cm3 {
                    let mut res = LRESULT(0);
                    let _ = c3.HandleMenuMsg2(WM_INITMENUPOPUP, wp, lp, Some(&mut res));
                } else if let Some(c2) = cm2 {
                    let _ = c2.HandleMenuMsg(WM_INITMENUPOPUP, wp, lp);
                }
            }
            children = unsafe { read_menu(cm, cm3, cm2, mii.hSubMenu, depth + 1) };
            // trim separators at the ends / empty submenus
            while children.first().is_some_and(|c| c.separator) {
                children.remove(0);
            }
            while children.last().is_some_and(|c| c.separator) {
                children.pop();
            }
            if children.is_empty() {
                continue;
            }
        }
        let id = if mii.hSubMenu.is_invalid() && (1..=0x7FFF).contains(&mii.wID) { mii.wID } else { 0 };
        if id == 0 && children.is_empty() {
            continue;
        }
        let verb = if id > 0 { verb_of(cm, (id - 1) as usize) } else { String::new() };
        out.push(ShellItem {
            id,
            label,
            hint,
            verb,
            icon: unsafe { bitmap_bgra(mii.hbmpItem) },
            enabled: !mii.fState.contains(MFS_DISABLED),
            separator: false,
            children,
        });
    }
    out
}

/// Reads a menu item bitmap as BGRA. Special `HBMMENU_*` values (owner-drawn callbacks,
/// system glyphs) yield `None`.
unsafe fn bitmap_bgra(hbm: windows::Win32::Graphics::Gdi::HBITMAP) -> Option<ShellIcon> {
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, GetDC, GetDIBits, GetObjectW, HGDIOBJ, ReleaseDC,
    };
    let raw = hbm.0 as isize;
    if (-1..=11).contains(&raw) {
        return None;
    }
    let mut bm = BITMAP::default();
    if unsafe { GetObjectW(HGDIOBJ(hbm.0), std::mem::size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut _)) } == 0 {
        return None;
    }
    let (w, h) = (bm.bmWidth, bm.bmHeight.abs());
    if w <= 0 || h <= 0 || w > 256 || h > 256 {
        return None;
    }
    let mut bi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut buf = vec![0u8; (w * h * 4) as usize];
    let hdc = unsafe { GetDC(None) };
    let lines = unsafe { GetDIBits(hdc, hbm, 0, h as u32, Some(buf.as_mut_ptr() as *mut _), &mut bi, DIB_RGB_COLORS) };
    unsafe {
        ReleaseDC(None, hdc);
    }
    if lines == 0 {
        return None;
    }
    // bitmaps without an alpha channel come back with alpha 0 everywhere
    if bm.bmBitsPixel < 32 || buf.chunks_exact(4).all(|p| p[3] == 0) {
        for p in buf.chunks_exact_mut(4) {
            p[3] = 255;
        }
    }
    Some(ShellIcon { width: w as u32, height: h as u32, bgra: buf })
}

fn run_menu(
    owner: HWND,
    cm: &IContextMenu,
    (x, y): (i32, i32),
    extended: bool,
    can_rename: bool,
    extra: &[ExtraItem],
) -> anyhow::Result<MenuOutcome> {
    unsafe {
        let hmenu = CreatePopupMenu()?;
        let mut flags = CMF_NORMAL | CMF_EXPLORE;
        if can_rename {
            flags |= CMF_CANRENAME;
        }
        if extended {
            flags |= CMF_EXTENDEDVERBS;
        }
        if let Err(e) = cm.QueryContextMenu(hmenu, 0, 1, 0x7FFF, flags).ok() {
            let _ = DestroyMenu(hmenu);
            return Err(e.into());
        }
        if !extra.is_empty() {
            for (i, it) in extra.iter().enumerate() {
                let label = crate::wide::wide(&it.label);
                let mut f = MF_BYPOSITION | MF_STRING;
                if !it.enabled {
                    f |= MF_GRAYED;
                }
                let _ = InsertMenuW(hmenu, i as u32, f, (EXTRA_BASE + it.id) as usize, PCWSTR(label.as_ptr()));
            }
            let _ = InsertMenuW(hmenu, extra.len() as u32, MF_BYPOSITION | MF_SEPARATOR, 0, PCWSTR::null());
        }

        let cm2: Option<IContextMenu2> = cm.cast().ok();
        let cm3: Option<IContextMenu3> = cm.cast().ok();
        MENU_HOOK.with(|h| {
            *h.borrow_mut() = Some(Box::new(move |msg: u32, wp: WPARAM, lp: LPARAM| -> Option<LRESULT> {
                match msg {
                    WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR => {
                        if let Some(c3) = &cm3 {
                            let mut res = LRESULT(0);
                            if c3.HandleMenuMsg2(msg, wp, lp, Some(&mut res)).is_ok() {
                                return Some(res);
                            }
                        } else if let Some(c2) = &cm2 {
                            if c2.HandleMenuMsg(msg, wp, lp).is_ok() {
                                return Some(LRESULT(0));
                            }
                        }
                        None
                    }
                    _ => None,
                }
            }));
        });

        let _ = SetForegroundWindow(owner);
        let cmd = TrackPopupMenuEx(hmenu, (TPM_RETURNCMD | TPM_RIGHTBUTTON).0, x, y, owner, None);
        let _ = PostMessageW(Some(owner), WM_NULL, WPARAM(0), LPARAM(0));
        MENU_HOOK.with(|h| *h.borrow_mut() = None);

        let id = cmd.0 as u32;
        let result = if id == 0 {
            Ok(MenuOutcome::Dismissed)
        } else if id >= EXTRA_BASE {
            Ok(MenuOutcome::Custom(id - EXTRA_BASE))
        } else {
            invoke_offset(owner, cm, id - 1, (x, y))
        };
        let _ = DestroyMenu(hmenu);
        result
    }
}

/// A plain popup menu of app items at `screen` (no shell items). Returns the chosen id.
/// Call on the shell STA thread with its owner window.
pub fn show_simple(owner: HWND, screen: (i32, i32), items: &[ExtraItem]) -> Option<u32> {
    unsafe {
        let hmenu = CreatePopupMenu().ok()?;
        for (i, it) in items.iter().enumerate() {
            if it.label == "-" {
                let _ = InsertMenuW(hmenu, i as u32, MF_BYPOSITION | MF_SEPARATOR, 0, PCWSTR::null());
                continue;
            }
            let label = crate::wide::wide(&it.label);
            let mut f = MF_BYPOSITION | MF_STRING;
            if !it.enabled {
                f |= MF_GRAYED;
            }
            let _ = InsertMenuW(hmenu, i as u32, f, (EXTRA_BASE + it.id) as usize, PCWSTR(label.as_ptr()));
        }
        let _ = SetForegroundWindow(owner);
        let cmd = TrackPopupMenuEx(hmenu, (TPM_RETURNCMD | TPM_RIGHTBUTTON).0, screen.0, screen.1, owner, None);
        let _ = PostMessageW(Some(owner), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(hmenu);
        let id = cmd.0 as u32;
        (id >= EXTRA_BASE).then(|| id - EXTRA_BASE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(clean_label("&Open"), ("Open".to_string(), None));
        assert_eq!(clean_label("Save && Close\tCtrl+S"), ("Save & Close".to_string(), Some("Ctrl+S".to_string())));
        assert_eq!(clean_label("  Gönder  "), ("Gönder".to_string(), None));
    }

    fn dump(items: &[ShellItem], depth: usize) {
        for i in items {
            if i.separator {
                println!("{}----", "  ".repeat(depth));
                continue;
            }
            println!(
                "{}[{}] {:?} verb={:?} icon={} enabled={}",
                "  ".repeat(depth),
                i.id,
                i.label,
                i.verb,
                i.icon.as_ref().map(|c| format!("{}x{}", c.width, c.height)).unwrap_or_default(),
                i.enabled
            );
            dump(&i.children, depth + 1);
        }
    }

    /// Manual: `RF_MENU_DUMP=<path> cargo test -p rf-win dump_shell_menu -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn dump_shell_menu() {
        let p = PathBuf::from(std::env::var("RF_MENU_DUMP").unwrap_or_else(|_| std::env::var("USERPROFILE").unwrap()));
        let (tx, rx) = std::sync::mpsc::channel();
        crate::sta::run(move |owner| {
            let t0 = std::time::Instant::now();
            let item = build(owner, &ShellTarget::Items(vec![p.clone()]), false).map(|(_, v)| v);
            let t1 = t0.elapsed();
            let bg = build(owner, &ShellTarget::Background(p.parent().unwrap().to_path_buf()), false).map(|(_, v)| v);
            let _ = tx.send((item.map_err(|e| e.to_string()), t1, bg.map_err(|e| e.to_string()), t0.elapsed()));
        });
        let (item, t1, bg, t2) = rx.recv_timeout(std::time::Duration::from_secs(30)).unwrap();
        println!("== item menu ({t1:?})");
        dump(&item.unwrap(), 0);
        println!("== background menu ({t2:?} total)");
        dump(&bg.unwrap(), 0);
    }
}
