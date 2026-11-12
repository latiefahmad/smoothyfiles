//! "Set as default file manager" (Files' Advanced setting), per-user (HKCU only, no admin):
//! folders and Win+E open in smoothyfiles. Same keys as Files' SetFilesAsDefault.reg. Archives
//! are offered as an app registration (see `register_app`): the user picks smoothyfiles for them
//! in Windows' Default apps.
//!
//! The app must hand non-folder arguments (e.g. `::{CLSID}` shell namespaces like Control Panel)
//! back to explorer.exe — see `rf-app` main.

use windows_registry::{CURRENT_USER, Key};

pub const CLASSES: &str = r"Software\Classes";
const WIN_E_CLSID: &str = r"CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}";
/// zip (CompressedFolder) and 7z, rar, tar, gz… (ArchiveFolder): their `open` verbs. Earlier
/// versions pointed these at smoothyfiles; no more: "Open with" then merged Windows' own handler
/// ("File Explorer") with ours into one entry with Explorer's name. Ours are removed.
const ARCHIVE_VERBS: [&str; 2] = [r"CompressedFolder\shell\open", r"ArchiveFolder\shell\open"];

fn set_command(base: &Key, sub: &str, command: &str) -> anyhow::Result<()> {
    let k = base.create(sub)?;
    k.set_string("", command)?;
    k.set_string("DelegateExecute", "")?;
    Ok(())
}

/// Registers `exe` under `classes_root` (normally `CLASSES`; tests pass a scratch key).
pub fn enable(exe: &str, classes_root: &str) -> anyhow::Result<()> {
    let base = CURRENT_USER.create(classes_root)?;
    let cmd = format!("\"{exe}\" \"%1\"");
    set_command(&base, r"Folder\shell\open\command", &cmd)?;
    set_command(&base, r"Folder\shell\explore\command", &cmd)?;
    set_command(&base, &format!(r"{WIN_E_CLSID}\shell\opennewwindow\command"), &format!("\"{exe}\""))?;
    remove_archive_verbs(classes_root);
    register_app(exe, classes_root)
}

/// Archive types smoothyfiles browses, offered in Windows' Default apps and "Open with". Which
/// app opens them is the user's choice (Windows keeps it signed; no program may set it).
pub const ARCHIVE_EXTS: [&str; 9] = [".zip", ".7z", ".rar", ".tar", ".tgz", ".txz", ".tzst", ".tbz2", ".tbz"];
const PROGID: &str = "smoothyfiles.Archive";
/// Name under RegisteredApplications (`ms-settings:defaultapps?registeredAppUser=smoothyfiles`).
pub const APP_NAME: &str = "smoothyfiles";

/// `Software` for `Software\Classes` (tests use a scratch root).
fn software_root(classes_root: &str) -> &str {
    classes_root.strip_suffix(r"\Classes").unwrap_or(classes_root)
}

/// Registers smoothyfiles as an app that opens archives (Default Programs capabilities): it
/// can then be picked in Settings → Default apps or "Open with → Always". Changes no default.
pub fn register_app(exe: &str, classes_root: &str) -> anyhow::Result<()> {
    let base = CURRENT_USER.create(classes_root)?;
    let p = base.create(PROGID)?;
    p.set_string("", "Sıkıştırılmış arşiv")?;
    // the zip folder icon, not ours: archives must still look like archives
    base.create(format!(r"{PROGID}\DefaultIcon"))?.set_expand_string("", r"%SystemRoot%\system32\zipfldr.dll")?;
    // a plain command: `DelegateExecute` (needed above to beat HKLM's) would make Windows list
    // this handler as File Explorer
    let _ = base.remove_tree(format!(r"{PROGID}\shell"));
    base.create(format!(r"{PROGID}\shell\open\command"))?.set_string("", format!("\"{exe}\" \"%1\""))?;
    // the name and icon "Open with" / Default apps show for this handler
    let app = base.create(format!(r"{PROGID}\Application"))?;
    app.set_string("ApplicationName", "smoothyfiles")?;
    app.set_string("ApplicationIcon", format!("\"{exe}\",0"))?;
    app.set_string("ApplicationCompany", "smoothyfiles")?;
    for e in ARCHIVE_EXTS {
        base.create(format!(r"{e}\OpenWithProgids"))?.set_string(PROGID, "")?;
    }
    let root = software_root(classes_root);
    let caps_path = format!(r"{root}\smoothyfiles\Capabilities");
    let caps = CURRENT_USER.create(&caps_path)?;
    caps.set_string("ApplicationName", "smoothyfiles")?;
    caps.set_string("ApplicationDescription", "Hızlı dosya yöneticisi")?;
    let fa = caps.create("FileAssociations")?;
    for e in ARCHIVE_EXTS {
        fa.set_string(e, PROGID)?;
    }
    CURRENT_USER.create(format!(r"{root}\RegisteredApplications"))?.set_string(APP_NAME, &caps_path)?;
    Ok(())
}

fn unregister_app(classes_root: &str) {
    let Ok(base) = CURRENT_USER.create(classes_root) else { return };
    let _ = base.remove_tree(PROGID);
    for e in ARCHIVE_EXTS {
        if let Ok(k) = base.open(format!(r"{e}\OpenWithProgids")) {
            let _ = k.remove_value(PROGID);
        }
    }
    let root = software_root(classes_root);
    let _ = CURRENT_USER.remove_tree(format!(r"{root}\smoothyfiles"));
    if let Ok(k) = CURRENT_USER.open(format!(r"{root}\RegisteredApplications")) {
        let _ = k.remove_value(APP_NAME);
    }
}

/// True when the archive app registration points at `exe`.
fn app_registered(exe: &str, classes_root: &str) -> bool {
    verb_command(classes_root, &format!(r"{PROGID}\shell\open")).is_some_and(|c| c == format!("\"{exe}\" \"%1\""))
        && CURRENT_USER.open(format!(r"{}\RegisteredApplications", software_root(classes_root))).and_then(|k| k.get_string(APP_NAME)).is_ok()
        // registrations before the name fix had DelegateExecute and no Application key
        && CURRENT_USER.open(format!(r"{classes_root}\{PROGID}\Application")).and_then(|k| k.get_string("ApplicationName")).is_ok()
        && CURRENT_USER.open(format!(r"{classes_root}\{PROGID}\shell\open\command")).and_then(|k| k.get_string("DelegateExecute")).is_err()
}

fn verb_command(classes_root: &str, verb: &str) -> Option<String> {
    CURRENT_USER.open(format!(r"{classes_root}\{verb}\command")).and_then(|k| k.get_string("")).ok()
}

fn ours(classes_root: &str, verb: &str) -> bool {
    verb_command(classes_root, verb).is_some_and(|v| v.to_lowercase().contains("smoothyfiles"))
}

/// Our old archive verb overrides go (another app's own ones stay).
fn remove_archive_verbs(classes_root: &str) -> bool {
    let Ok(base) = CURRENT_USER.create(classes_root) else { return false };
    let mut removed = false;
    for v in ARCHIVE_VERBS {
        if ours(classes_root, v) {
            removed |= base.remove_tree(v).is_ok();
        }
    }
    removed
}

/// Brings an older registration up to date: the archive app registration added, the old archive
/// verb overrides removed. True when it changed something.
pub fn add_archives(exe: &str, classes_root: &str) -> bool {
    if !is_enabled(classes_root) {
        return false;
    }
    let removed = remove_archive_verbs(classes_root);
    let added = !app_registered(exe, classes_root) && register_app(exe, classes_root).is_ok();
    removed || added
}

/// Removes our overrides (the system defaults under HKLM take over again).
pub fn disable(classes_root: &str) -> anyhow::Result<()> {
    let base = CURRENT_USER.create(classes_root)?;
    for sub in [r"Folder\shell\open", r"Folder\shell\explore", WIN_E_CLSID] {
        // missing keys are fine
        let _ = base.remove_tree(sub);
    }
    remove_archive_verbs(classes_root);
    unregister_app(classes_root);
    Ok(())
}

pub fn is_enabled(classes_root: &str) -> bool {
    CURRENT_USER
        .open(format!(r"{classes_root}\Folder\shell\open\command"))
        .and_then(|k| k.get_string(""))
        .map(|v| v.to_lowercase().contains("smoothyfiles"))
        .unwrap_or(false)
}

/// The exe our `Folder\shell\open` command points at (None when not enabled).
pub fn registered_exe(classes_root: &str) -> Option<std::path::PathBuf> {
    let v = CURRENT_USER.open(format!(r"{classes_root}\Folder\shell\open\command")).and_then(|k| k.get_string("")).ok()?;
    let v = v.trim();
    let exe = match v.strip_prefix('"') {
        Some(rest) => rest.split('"').next()?,
        None => v.split(' ').next()?,
    };
    exe.to_lowercase().contains("smoothyfiles").then(|| exe.into())
}

/// While we are the default, a registered exe that no longer exists (moved, rebuilt elsewhere,
/// its folder cleaned) would leave Windows unable to open any folder: point it at `current`.
/// Returns true when it re-registered.
pub fn heal(current: &str, classes_root: &str) -> bool {
    match registered_exe(classes_root) {
        Some(old) if !old.exists() && std::path::Path::new(current).exists() => enable(current, classes_root).is_ok(),
        _ => false,
    }
}

/// True when the argument should be handed back to Explorer (shell namespaces, non-folders).
pub fn should_forward_to_explorer(arg: &str) -> bool {
    let a = arg.trim().trim_matches('"');
    !a.is_empty() && (a.starts_with("::") || a.starts_with("shell:") || !std::path::Path::new(a).is_dir())
}

/// Explorer opens folders through the same `Folder\shell\open` verb we take over, so an
/// argument we hand to it can come straight back (a server root like `\\nas`, a folder that
/// looked like a file). Forwards are remembered for a few seconds to break such a loop.
fn forward_log() -> std::path::PathBuf {
    std::env::temp_dir().join("smoothyfiles-forward.txt")
}

fn now_ms() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis())
}

/// True when `arg` was handed to Explorer within the last few seconds.
pub fn forwarded_recently(arg: &str) -> bool {
    forwarded_within(&forward_log(), arg, now_ms(), 5_000)
}

/// Remembers that `arg` is being handed to Explorer.
pub fn note_forward(arg: &str) {
    note_in(&forward_log(), arg, now_ms());
}

fn forwarded_within(log: &std::path::Path, arg: &str, now: u128, window_ms: u128) -> bool {
    let Ok(text) = std::fs::read_to_string(log) else { return false };
    text.lines().filter_map(|l| l.split_once('\t')).any(|(t, a)| a == arg && t.parse::<u128>().is_ok_and(|t| now.saturating_sub(t) <= window_ms))
}

fn note_in(log: &std::path::Path, arg: &str, now: u128) {
    let old = std::fs::read_to_string(log).unwrap_or_default();
    let mut lines: Vec<&str> = old.lines().filter(|l| !l.is_empty()).collect();
    let keep = lines.len().saturating_sub(7);
    lines.drain(..keep);
    let mut out: String = lines.iter().map(|l| format!("{l}\n")).collect();
    out.push_str(&format!("{now}\t{arg}\n"));
    let _ = std::fs::write(log, out);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enable_disable_in_scratch_key() {
        let root = format!(r"Software\smoothyfiles-test-{}\Classes", std::process::id());
        enable(r"C:\x\smoothyfiles.exe", &root).unwrap();
        assert!(is_enabled(&root));
        let k = CURRENT_USER.open(format!(r"{root}\{WIN_E_CLSID}\shell\opennewwindow\command")).unwrap();
        assert_eq!(k.get_string("").unwrap(), "\"C:\\x\\smoothyfiles.exe\"");
        // Windows' own archive handlers stay Explorer's
        assert!(!ARCHIVE_VERBS.iter().any(|v| ours(&root, v)));
        assert!(app_registered(r"C:\x\smoothyfiles.exe", &root));
        let fa = CURRENT_USER.open(format!(r"Software\smoothyfiles-test-{}\smoothyfiles\Capabilities\FileAssociations", std::process::id())).unwrap();
        assert_eq!(fa.get_string(".zip").unwrap(), PROGID);
        // an older registration: its archive verb overrides go
        let base = CURRENT_USER.create(&root).unwrap();
        set_command(&base, &format!(r"{}\command", ARCHIVE_VERBS[1]), r#""C:\x\smoothyfiles.exe" "%1""#).unwrap();
        assert!(add_archives(r"C:\x\smoothyfiles.exe", &root));
        assert!(!ours(&root, ARCHIVE_VERBS[1]));
        assert!(!add_archives(r"C:\x\smoothyfiles.exe", &root)); // nothing left to do
        disable(&root).unwrap();
        assert!(!is_enabled(&root));
        assert!(!ARCHIVE_VERBS.iter().any(|v| ours(&root, v)));
        assert!(!app_registered(r"C:\x\smoothyfiles.exe", &root));
        assert!(CURRENT_USER.open(format!(r"{root}\{PROGID}")).is_err());
        // another app's archive verb survives
        set_command(&base, &format!(r"{}\command", ARCHIVE_VERBS[0]), r"C:\7z\7zFM.exe %1").unwrap();
        disable(&root).unwrap();
        assert!(CURRENT_USER.open(format!(r"{root}\{}\command", ARCHIVE_VERBS[0])).is_ok());
        let _ = CURRENT_USER.remove_tree(format!(r"Software\smoothyfiles-test-{}", std::process::id()));
        assert!(should_forward_to_explorer("::{26EE0668-A00A-44D7-9371-BEB064C98683}"));
        assert!(!should_forward_to_explorer(r"C:\Windows"));
    }

    #[test]
    fn heals_a_missing_exe() {
        let root = format!(r"Software\smoothyfiles-heal-{}\Classes", std::process::id());
        let me = std::env::current_exe().unwrap().display().to_string();
        enable(r"C:\yok\smoothyfiles.exe", &root).unwrap();
        assert_eq!(registered_exe(&root), Some(std::path::PathBuf::from(r"C:\yok\smoothyfiles.exe")));
        // current_exe is a test binary: name it like the app for the "is it ours" check
        let fake = std::env::temp_dir().join(format!("smoothyfiles-heal-{}.exe", std::process::id()));
        std::fs::copy(&me, &fake).unwrap();
        assert!(heal(&fake.display().to_string(), &root));
        assert_eq!(registered_exe(&root), Some(fake.clone()));
        assert!(!heal(&fake.display().to_string(), &root)); // already fine
        let _ = CURRENT_USER.remove_tree(format!(r"Software\smoothyfiles-heal-{}", std::process::id()));
        let _ = std::fs::remove_file(&fake);
    }

    #[test]
    fn forward_loop_guard() {
        let log = std::env::temp_dir().join(format!("rf-fwd-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&log);
        assert!(!forwarded_within(&log, r"\\nas", 1_000, 5_000));
        note_in(&log, r"\\nas", 1_000);
        assert!(forwarded_within(&log, r"\\nas", 4_000, 5_000));
        assert!(!forwarded_within(&log, r"\\nas", 9_000, 5_000)); // long ago: forward again
        assert!(!forwarded_within(&log, r"\\other", 2_000, 5_000));
        for i in 0..20 {
            note_in(&log, &format!("x{i}"), 2_000 + i);
        }
        assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 8); // bounded
        let _ = std::fs::remove_file(&log);
    }
}
