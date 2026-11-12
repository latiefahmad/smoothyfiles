//! "smoothyfiles in Open / Save dialogs": registers smoothyfiles_dialog.dll (crates/rf-dialog) as the
//! COM server of Windows' FileOpenDialog and FileSaveDialog classes, per user (HKCU, no admin).
//! Apps running as administrator don't read per-user COM registrations and keep Windows' own
//! dialog; so do 32-bit apps (nothing is registered under WOW6432Node).

use windows_registry::CURRENT_USER;

pub const CLASSES: &str = r"Software\Classes";
pub const DLL_NAME: &str = "smoothyfiles_dialog.dll";
const CLSIDS: [&str; 2] = ["{DC1C5A9C-E88A-4dde-A5A1-60F82A20AEF7}", "{C0B4E2F3-BA21-4773-8DBA-335EC946EB8B}"];

fn server_key(classes_root: &str, clsid: &str) -> String {
    format!(r"{classes_root}\CLSID\{clsid}\InprocServer32")
}

fn registered(classes_root: &str, clsid: &str) -> Option<String> {
    CURRENT_USER.open(server_key(classes_root, clsid)).and_then(|k| k.get_string("")).ok()
}

fn ours(path: &str) -> bool {
    path.to_lowercase().ends_with(DLL_NAME)
}

/// Points both classes at `dll`.
pub fn enable(dll: &str, classes_root: &str) -> anyhow::Result<()> {
    anyhow::ensure!(std::path::Path::new(dll).exists(), "{dll} not found");
    for c in CLSIDS {
        let k = CURRENT_USER.create(server_key(classes_root, c))?;
        k.set_string("", dll)?;
        k.set_string("ThreadingModel", "Apartment")?;
    }
    Ok(())
}

/// Removes our registrations (Windows' own dialog comes back); someone else's stays.
pub fn disable(classes_root: &str) -> anyhow::Result<()> {
    for c in CLSIDS {
        if registered(classes_root, c).is_some_and(|p| ours(&p)) {
            let _ = CURRENT_USER.remove_tree(format!(r"{classes_root}\CLSID\{c}"));
        }
    }
    Ok(())
}

pub fn is_enabled(classes_root: &str) -> bool {
    CLSIDS.iter().all(|c| registered(classes_root, c).is_some_and(|p| ours(&p)))
}

/// While enabled: registers `dll` if the registration points anywhere else (a newer build, a
/// new install). True when it changed something.
pub fn point_to(dll: &str, classes_root: &str) -> bool {
    let differs = CLSIDS.iter().any(|c| registered(classes_root, c).is_none_or(|p| !p.eq_ignore_ascii_case(dll)));
    is_enabled(classes_root) && differs && enable(dll, classes_root).is_ok()
}

/// While enabled, a DLL that moved (reinstalled elsewhere, rebuilt) would leave every app's
/// file dialog broken: point the registration at `dll`. True when it changed something.
pub fn heal(dll: &str, classes_root: &str) -> bool {
    let stale = CLSIDS.iter().any(|c| registered(classes_root, c).is_some_and(|p| ours(&p) && !p.eq_ignore_ascii_case(dll) && !std::path::Path::new(&p).exists()));
    stale && std::path::Path::new(dll).exists() && enable(dll, classes_root).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enable_disable_heal_in_scratch_key() {
        let root = format!(r"Software\smoothyfiles-dlg-test-{}\Classes", std::process::id());
        let dir = std::env::temp_dir().join(format!("rf-dlg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dll = dir.join(DLL_NAME);
        std::fs::write(&dll, b"x").unwrap();
        let dll = dll.to_string_lossy().into_owned();
        assert!(!is_enabled(&root));
        enable(&dll, &root).unwrap();
        assert!(is_enabled(&root));
        assert_eq!(CURRENT_USER.open(server_key(&root, CLSIDS[1])).unwrap().get_string("ThreadingModel").unwrap(), "Apartment");
        // the DLL moved: heal points at the new one
        let moved = dir.join("yeni").join(DLL_NAME);
        std::fs::create_dir_all(moved.parent().unwrap()).unwrap();
        std::fs::rename(&dll, &moved).unwrap();
        let moved = moved.to_string_lossy().into_owned();
        assert!(heal(&moved, &root));
        assert_eq!(registered(&root, CLSIDS[0]).as_deref(), Some(moved.as_str()));
        assert!(!heal(&moved, &root));
        disable(&root).unwrap();
        assert!(!is_enabled(&root));
        // someone else's server stays
        let k = CURRENT_USER.create(server_key(&root, CLSIDS[0])).unwrap();
        k.set_string("", r"C:\Other\dialog.dll").unwrap();
        disable(&root).unwrap();
        assert!(registered(&root, CLSIDS[0]).is_some());
        let _ = CURRENT_USER.remove_tree(format!(r"Software\smoothyfiles-dlg-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
