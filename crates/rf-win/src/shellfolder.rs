//! Virtual shell folders: places in the shell namespace that have no file-system path (an MTP
//! phone under This PC, Phone Link's mobile devices, Network, a library…). Items are addressed
//! by their desktop-absolute parsing name, which `SHCreateItemFromParsingName` takes back, so a
//! location can be stored, restored and navigated to later.
//!
//! Everything here blocks (device I/O, shell extensions): call from a worker thread. COM is
//! initialized per call (see `shell::ensure_com`).

use std::path::{Path, PathBuf};

use windows::Win32::Foundation::FILETIME;
use windows::Win32::Storage::EnhancedStorage::{PKEY_DateModified, PKEY_Size};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::SystemServices::{SFGAO_FILESYSTEM, SFGAO_FLAGS, SFGAO_FOLDER, SFGAO_STREAM};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    BHID_EnumItems, IEnumShellItems, IShellItem, IShellItem2, SEE_MASK_FLAG_NO_UI, SEE_MASK_IDLIST, SEE_MASK_NOASYNC,
    SHCreateItemFromParsingName, SHELLEXECUTEINFOW, SHParseDisplayName,
    ShellExecuteExW, SIGDN, SIGDN_DESKTOPABSOLUTEPARSING, SIGDN_FILESYSPATH, SIGDN_NORMALDISPLAY,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{Interface, PCWSTR};

use crate::shell::ensure_com;
use crate::wide::wide;

/// "This PC": its non-file-system children are the devices (phones, cameras).
pub const THIS_PC: &str = "::{20D04FE0-3AEA-1069-A2D8-08002B30309D}";

/// One child of a shell folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellChild {
    /// What the shell shows ("Internal storage", "DCIM", "IMG_0001.jpg").
    pub name: String,
    /// Desktop-absolute parsing name: navigate to it, copy from it, open it.
    pub parsing: String,
    /// A folder to browse into (a `.zip` is a shell folder too, but a stream: not counted).
    pub is_folder: bool,
    /// The real file-system path when the item is one (`SFGAO_FILESYSTEM`).
    pub fs_path: Option<PathBuf>,
    pub size: Option<u64>,
    /// Unix time in milliseconds.
    pub modified_ms: Option<i64>,
}

/// A resolved shell folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place {
    pub parsing: String,
    pub name: String,
    pub fs_path: Option<PathBuf>,
}

fn display(item: &IShellItem, sigdn: SIGDN) -> Option<String> {
    unsafe {
        let p = item.GetDisplayName(sigdn).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

fn attrs(item: &IShellItem, mask: SFGAO_FLAGS) -> SFGAO_FLAGS {
    unsafe { item.GetAttributes(mask).unwrap_or_default() }
}

fn filetime_ms(ft: FILETIME) -> i64 {
    let t = (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime);
    (t / 10_000) as i64 - 11_644_473_600_000
}

fn bind(parsing: &str) -> anyhow::Result<IShellItem> {
    ensure_com();
    let w = wide(parsing.trim());
    Ok(unsafe { SHCreateItemFromParsingName(PCWSTR(w.as_ptr()), None)? })
}

fn child_of(item: &IShellItem) -> Option<ShellChild> {
    let parsing = display(item, SIGDN_DESKTOPABSOLUTEPARSING)?;
    let name = display(item, SIGDN_NORMALDISPLAY).unwrap_or_else(|| parsing.clone());
    let a = attrs(item, SFGAO_FOLDER | SFGAO_FILESYSTEM | SFGAO_STREAM);
    let is_folder = a.contains(SFGAO_FOLDER) && !a.contains(SFGAO_STREAM);
    let fs_path = a.contains(SFGAO_FILESYSTEM).then(|| display(item, SIGDN_FILESYSPATH)).flatten().map(PathBuf::from);
    let (mut size, mut modified_ms) = (None, None);
    if let Ok(i2) = item.cast::<IShellItem2>() {
        if !is_folder {
            size = unsafe { i2.GetUInt64(&PKEY_Size).ok() };
        }
        modified_ms = unsafe { i2.GetFileTime(&PKEY_DateModified).ok() }.map(filetime_ms).filter(|&m| m > 0);
    }
    Some(ShellChild { name, parsing, is_folder, fs_path, size, modified_ms })
}

fn enumerate(item: &IShellItem, cancel: &dyn Fn() -> bool, on_batch: &mut dyn FnMut(Vec<ShellChild>)) -> anyhow::Result<usize> {
    let en: IEnumShellItems = unsafe { item.BindToHandler(None, &BHID_EnumItems)? };
    let mut total = 0;
    loop {
        if cancel() {
            break;
        }
        let mut buf: [Option<IShellItem>; 32] = Default::default();
        let mut got = 0u32;
        // S_FALSE with fewer items than asked = the end
        let _ = unsafe { en.Next(&mut buf, Some(&mut got)) };
        if got == 0 {
            break;
        }
        let batch: Vec<ShellChild> = buf.iter().take(got as usize).flatten().filter_map(child_of).collect();
        total += batch.len();
        if !batch.is_empty() {
            on_batch(batch);
        }
        if got < 32 {
            break;
        }
    }
    Ok(total)
}

/// Lists the children of the shell folder `parsing`, in batches as the shell hands them out
/// (a slow device shows its first items while the rest is still coming). Stops early when
/// `cancel` turns true. Returns the number of items.
pub fn list(parsing: &str, cancel: &dyn Fn() -> bool, on_batch: &mut dyn FnMut(Vec<ShellChild>)) -> anyhow::Result<usize> {
    let item = bind(parsing)?;
    enumerate(&item, cancel, on_batch)
}

/// The shell's name for a folder (what an address bar would show).
pub fn display_name(parsing: &str) -> Option<String> {
    display(&bind(parsing).ok()?, SIGDN_NORMALDISPLAY)
}

/// Folder one level up in the namespace, if there is one (`None` at the desktop).
pub fn parent(parsing: &str) -> Option<Place> {
    let item = bind(parsing).ok()?;
    let up = unsafe { item.GetParent().ok()? };
    place_of(&up)
}

fn place_of(item: &IShellItem) -> Option<Place> {
    let parsing = display(item, SIGDN_DESKTOPABSOLUTEPARSING)?;
    let name = display(item, SIGDN_NORMALDISPLAY).unwrap_or_else(|| parsing.clone());
    let fs_path = attrs(item, SFGAO_FILESYSTEM).contains(SFGAO_FILESYSTEM).then(|| display(item, SIGDN_FILESYSPATH)).flatten().map(PathBuf::from);
    Some(Place { parsing, name, fs_path })
}

/// Namespaces Explorer shows with a UI of its own (Control Panel and its applets, printers,
/// network connections, the apps folder…): not browsable as a plain list, left to Explorer.
const APPLETS: &[&str] = &[
    "26ee0668-a00a-44d7-9371-beb064c98683", // Control Panel (all categories)
    "21ec2020-3aea-1069-a2dd-08002b30309d", // Control Panel (all items)
    "5399e694-6ce5-4d6c-8fce-1d8870fdcba0", // Control Panel (category view)
    "ed7ba470-8e54-465e-825c-99712043e01c", // All Tasks ("God mode")
    "2227a280-3aea-1069-a2de-08002b30309d", // Printers
    "7007acc7-3202-11d1-aad2-00805fc1270e", // Network Connections
    "7b81be6a-ce2b-4676-a29e-eb907a5126c5", // Programs and Features
    "4234d49b-0245-4df3-b780-3893943456e1", // Apps (AppsFolder)
    "e7de9b1a-7533-4556-b2f7-9b3ac4b0cb2d", // Windows Update (legacy)
    "9343812e-1c37-4a49-a12e-4b2d810d956b", // Search
];

/// True when `parsing` is (inside) one of the applet namespaces.
pub fn is_applet(parsing: &str) -> bool {
    let p = parsing.to_ascii_lowercase();
    APPLETS.iter().any(|g| p.contains(g))
}

/// Resolves a `::{GUID}` / `shell:` / parsing-name argument to a shell folder that smoothyfiles
/// can list itself: it must be a folder, not an applet namespace, and its enumerator must
/// open. Folders on the file system come back with their `fs_path` (the caller opens those as
/// ordinary folders). `None` = leave it to Explorer.
pub fn browsable(s: &str) -> Option<Place> {
    let item = bind(s).ok()?;
    if !attrs(&item, SFGAO_FOLDER).contains(SFGAO_FOLDER) {
        return None;
    }
    let place = place_of(&item)?;
    if is_applet(&place.parsing) || is_applet(s) {
        return None;
    }
    // the enumerator has to open (a namespace extension that cannot is no folder to us)
    unsafe { item.BindToHandler::<_, IEnumShellItems>(None, &BHID_EnumItems).ok()? };
    Some(place)
}

/// Devices under This PC that are not drives: MTP phones and cameras, Phone Link's mobile
/// devices. Empty when none is connected (the usual case).
pub fn devices() -> Vec<ShellChild> {
    let mut out = Vec::new();
    let Ok(pc) = bind(THIS_PC) else { return out };
    let _ = enumerate(&pc, &|| false, &mut |b| out.extend(b.into_iter().filter(|c| c.fs_path.is_none() && c.is_folder)));
    out.retain(|c| !is_applet(&c.parsing));
    out
}

/// Copies shell items (given by parsing name) into the real folder `dest` with the shell's own
/// copy engine, which knows how to pull a file off a phone. `Ok(false)` = cancelled.
pub fn copy_out(sources: &[String], dest: &Path) -> anyhow::Result<bool> {
    use windows::Win32::UI::Shell::FOF_NOCONFIRMMKDIR;
    let src: Vec<PathBuf> = sources.iter().map(PathBuf::from).collect();
    match crate::fileops::shell_copy(&src, dest, FOF_NOCONFIRMMKDIR.0 as u32) {
        Ok(()) => Ok(true),
        Err(e) if e.to_string().contains("aborted") => Ok(false),
        Err(e) => Err(e),
    }
}

/// Opens an item with the shell's default verb (what a double-click in Explorer does).
pub fn open(parsing: &str) -> anyhow::Result<()> {
    ensure_com();
    let w = wide(parsing.trim());
    let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
    unsafe {
        SHParseDisplayName(PCWSTR(w.as_ptr()), None, &mut pidl, 0, None)?;
        let mut info = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_IDLIST | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
            lpIDList: pidl as *mut _,
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };
        let r = ShellExecuteExW(&mut info);
        CoTaskMemFree(Some(pidl as *const _));
        r?;
    }
    Ok(())
}

/// Last part of a parsing name, for a title when the shell cannot be asked: `...\Documents`
/// → `Documents`.
pub fn last_segment(parsing: &str) -> &str {
    let t = parsing.trim_end_matches('\\');
    t.rsplit('\\').next().unwrap_or(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIBRARIES: &str = "::{031E4825-7B94-4DC3-B131-E946B44C8DD5}";
    const NETWORK: &str = "::{F02C1A0D-BE21-4350-88B0-7367FC96EF3C}";

    #[test]
    fn libraries_list_and_parsing_names_round_trip() {
        let mut got = Vec::new();
        let n = list(LIBRARIES, &|| false, &mut |b| got.extend(b)).expect("libraries enumerate");
        assert_eq!(n, got.len());
        for c in &got {
            assert!(!c.name.is_empty() && !c.parsing.is_empty(), "{c:?}");
            // what we hand out can be bound again
            assert_eq!(display_name(&c.parsing).as_deref(), Some(c.name.as_str()), "{c:?}");
        }
        eprintln!("libraries: {:?}", got.iter().map(|c| (&c.name, &c.parsing, c.is_folder)).collect::<Vec<_>>());
    }

    #[test]
    fn this_pc_children_and_devices_do_not_panic() {
        let mut got = Vec::new();
        list(THIS_PC, &|| false, &mut |b| got.extend(b)).expect("this pc");
        assert!(got.iter().any(|c| c.fs_path.is_some()), "a drive is expected: {got:?}");
        for d in devices() {
            assert!(d.fs_path.is_none() && d.is_folder, "{d:?}");
        }
    }

    #[test]
    fn network_enumerates_or_fails_cleanly() {
        let mut n = 0;
        let _ = list(NETWORK, &|| false, &mut |b| n += b.len());
        eprintln!("network items: {n}");
    }

    #[test]
    fn cancel_stops_early() {
        let mut batches = 0;
        let _ = list(THIS_PC, &|| true, &mut |_| batches += 1);
        assert_eq!(batches, 0);
    }

    #[test]
    fn applets_are_left_to_explorer() {
        assert!(is_applet("::{26EE0668-A00A-44D7-9371-BEB064C98683}"));
        assert!(is_applet("::{26EE0668-A00A-44D7-9371-BEB064C98683}\\0\\::{21EC2020-3AEA-1069-A2DD-08002B30309D}"));
        assert!(!is_applet(LIBRARIES));
        assert!(browsable("::{26EE0668-A00A-44D7-9371-BEB064C98683}").is_none());
        assert!(browsable("shell:ControlPanelFolder").is_none());
        let lib = browsable(LIBRARIES).expect("libraries browse");
        assert!(!lib.name.is_empty());
        assert!(browsable("::{00000000-0000-0000-0000-000000000000}").is_none());
    }

    #[test]
    fn parent_goes_up_the_namespace() {
        let mut got = Vec::new();
        list(LIBRARIES, &|| false, &mut |b| got.extend(b)).unwrap();
        if let Some(c) = got.first() {
            let up = parent(&c.parsing).expect("a library has a parent");
            assert!(!up.name.is_empty());
        }
    }

    /// A `.zip` is a shell folder whose items have no file-system path: the same shape as a
    /// phone's. Lists it, copies an item out with the shell's engine, opens nothing.
    #[test]
    fn items_without_a_path_copy_out() {
        let dir = std::env::temp_dir().join(format!("rf-shellfolder-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src").join("hello.txt"), b"hello from a shell folder").unwrap();
        let zip = dir.join("t.zip");
        let status = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &format!("Compress-Archive -Path '{}' -DestinationPath '{}'", dir.join("src").join("hello.txt").display(), zip.display())])
            .status()
            .expect("powershell");
        assert!(status.success());
        let mut got = Vec::new();
        list(&zip.to_string_lossy(), &|| false, &mut |b| got.extend(b)).expect("zip enumerates");
        let item = got.iter().find(|c| c.name.starts_with("hello")).unwrap_or_else(|| panic!("hello.txt in {got:?}"));
        assert!(item.fs_path.is_none(), "{item:?}");
        assert!(!item.is_folder);
        assert_eq!(item.size, Some(25), "{item:?}");
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();
        assert!(copy_out(&[item.parsing.clone()], &out).expect("copy out"));
        let copied: Vec<_> = std::fs::read_dir(&out).unwrap().map(|e| e.unwrap().path()).collect();
        assert_eq!(copied.len(), 1, "{copied:?}");
        assert_eq!(std::fs::read(&copied[0]).unwrap(), b"hello from a shell folder");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn last_segment_of_parsing_names() {
        assert_eq!(last_segment(r"::{20D04FE0-3AEA-1069-A2D8-08002B30309D}\Foo"), "Foo");
        assert_eq!(last_segment("Foo"), "Foo");
    }
}
