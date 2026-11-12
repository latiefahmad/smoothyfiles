//! Who may do what with a file or folder (its DACL), for the Properties window's Security tab.
//! Read-only: the owner and the access-control list are read and summed up per principal; nothing
//! is ever written back.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Result, anyhow};
use windows::Win32::Foundation::{HLOCAL, LocalFree, WIN32_ERROR};
use windows::Win32::NetworkManagement::NetManagement::{NetApiBufferFree, NetUserGetInfo, USER_INFO_10};
use windows::Win32::Security::Authorization::{ConvertSidToStringSidW, GetNamedSecurityInfoW, SE_FILE_OBJECT};
use windows::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, GetAce, GetSecurityDescriptorControl, LookupAccountSidW, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
    PSID, SID_NAME_USE, SidTypeUser,
};
use windows::core::{PCWSTR, PWSTR};

use crate::wide::{from_wide, wide};

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// What Windows shows in the list: "SYSTEM", "Administrators (PC\\Administrators)", "Ozer (PC\\ozero)", "Authenticated Users"; an unresolvable SID shows as the SID string.
    pub name: String,
    /// The SID as a string ("S-1-5-18").
    pub sid: String,
    /// Access mask granted by allow ACEs for this principal (all its allow ACEs OR-ed together, inherited or not).
    pub allow: u32,
    /// Access mask of deny ACEs, OR-ed.
    pub deny: u32,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Security {
    /// Owner display name (same format as Entry::name), None if not readable.
    pub owner: Option<String>,
    /// One entry per principal, in the DACL's order of first appearance.
    pub entries: Vec<Entry>,
    /// The DACL is null (everyone has full access).
    pub null_dacl: bool,
    /// Inheritance from the parent is turned off (SE_DACL_PROTECTED).
    pub protected: bool,
}

const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const ACCESS_DENIED_ACE_TYPE: u8 = 1;
const INHERIT_ONLY_ACE: u8 = 0x08;
const SE_DACL_PROTECTED: u16 = 0x1000;

/// The SID text ("S-1-5-18") of a SID.
unsafe fn sid_string(sid: PSID) -> Option<String> {
    let mut p = PWSTR::null();
    unsafe {
        ConvertSidToStringSidW(sid, &mut p).ok()?;
        let s = from_wide(std::slice::from_raw_parts(p.0, wcslen(p.0)));
        let _ = LocalFree(Some(HLOCAL(p.0.cast())));
        Some(s)
    }
}

unsafe fn wcslen(p: *const u16) -> usize {
    let mut n = 0;
    unsafe {
        while *p.add(n) != 0 {
            n += 1;
        }
    }
    n
}

/// The way Explorer's list spells a principal. Well-known names stand alone ("SYSTEM", "Everyone");
/// groups and accounts of this PC or a domain get their domain in brackets, and a local user's
/// full name (when it has one) comes first: "Ozer (PC\ozero)".
unsafe fn display_name(sid: PSID, sid_text: &str) -> String {
    let mut name = vec![0u16; 256];
    let mut domain = vec![0u16; 256];
    let (mut nl, mut dl) = (name.len() as u32, domain.len() as u32);
    let mut kind = SID_NAME_USE(0);
    let ok = unsafe { LookupAccountSidW(PCWSTR::null(), sid, Some(PWSTR(name.as_mut_ptr())), &mut nl, Some(PWSTR(domain.as_mut_ptr())), &mut dl, &mut kind) };
    if ok.is_err() {
        return sid_text.to_string();
    }
    let name = from_wide(&name);
    let domain = from_wide(&domain);
    if name.is_empty() {
        return sid_text.to_string();
    }
    if domain.is_empty() || domain.eq_ignore_ascii_case("NT AUTHORITY") || domain.eq_ignore_ascii_case("NT SERVICE") || domain.eq_ignore_ascii_case("Window Manager") {
        return name;
    }
    let computer = std::env::var("COMPUTERNAME").unwrap_or_default();
    // BUILTIN groups are shown under the PC's own name, like Explorer does
    let shown_domain = if domain.eq_ignore_ascii_case("BUILTIN") && !computer.is_empty() { computer.clone() } else { domain.clone() };
    let mut first = name.clone();
    if kind == SidTypeUser && domain.eq_ignore_ascii_case(&computer) {
        // a local account: use its full name if it has one (a domain account would mean a network call)
        if let Some(full) = unsafe { local_full_name(&name) } {
            first = full;
        }
    }
    format!("{first} ({shown_domain}\\{name})")
}

/// The "Full name" field of a local user, if set.
unsafe fn local_full_name(user: &str) -> Option<String> {
    let u = wide(user);
    let mut buf: *mut u8 = std::ptr::null_mut();
    unsafe {
        if NetUserGetInfo(PCWSTR::null(), PCWSTR(u.as_ptr()), 10, &mut buf) != 0 || buf.is_null() {
            return None;
        }
        let info = &*(buf as *const USER_INFO_10);
        let full = if info.usri10_full_name.is_null() { String::new() } else { from_wide(std::slice::from_raw_parts(info.usri10_full_name.0, wcslen(info.usri10_full_name.0))) };
        let _ = NetApiBufferFree(Some(buf.cast()));
        let full = full.trim().to_string();
        (!full.is_empty()).then_some(full)
    }
}

/// Reads the owner and DACL of a file or folder.
pub fn read(path: &Path) -> Result<Security> {
    let name = wide(path);
    let mut owner = PSID::default();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut sd = PSECURITY_DESCRIPTOR::default();
    unsafe {
        let r: WIN32_ERROR = GetNamedSecurityInfoW(PCWSTR(name.as_ptr()), SE_FILE_OBJECT, OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION, Some(&mut owner), None, Some(&mut dacl), None, &mut sd);
        if r.0 != 0 {
            return Err(anyhow!("could not read the permissions (error {})", r.0));
        }
        let out = read_descriptor(owner, dacl, sd);
        let _ = LocalFree(Some(HLOCAL(sd.0)));
        Ok(out)
    }
}

unsafe fn read_descriptor(owner: PSID, dacl: *mut ACL, sd: PSECURITY_DESCRIPTOR) -> Security {
    let mut out = Security::default();
    let mut names: HashMap<String, String> = HashMap::new();
    let mut resolve = |sid: PSID| -> Option<(String, String)> {
        let text = unsafe { sid_string(sid)? };
        let shown = names.entry(text.clone()).or_insert_with(|| unsafe { display_name(sid, &text) }).clone();
        Some((text, shown))
    };
    if !owner.0.is_null() {
        out.owner = resolve(owner).map(|(_, n)| n);
    }
    let mut control = 0u16;
    let mut rev = 0u32;
    if unsafe { GetSecurityDescriptorControl(sd, &mut control, &mut rev) }.is_ok() {
        out.protected = control & SE_DACL_PROTECTED != 0;
    }
    if dacl.is_null() {
        out.null_dacl = true;
        return out;
    }
    let count = unsafe { (*dacl).AceCount };
    let mut index: HashMap<String, usize> = HashMap::new();
    for i in 0..count as u32 {
        let mut p: *mut core::ffi::c_void = std::ptr::null_mut();
        if unsafe { GetAce(dacl, i, &mut p) }.is_err() || p.is_null() {
            continue;
        }
        // plain allow / deny ACEs share one layout: header, mask, SID; object and callback ACEs are skipped
        let (ty, flags) = unsafe { ((*(p as *const u8)), *(p as *const u8).add(1)) };
        if (ty != ACCESS_ALLOWED_ACE_TYPE && ty != ACCESS_DENIED_ACE_TYPE) || flags & INHERIT_ONLY_ACE != 0 {
            continue;
        }
        let mask = unsafe { *((p as *const u8).add(4) as *const u32) };
        let sid = PSID(unsafe { (p as *mut u8).add(8) }.cast());
        let Some((text, shown)) = resolve(sid) else { continue };
        let at = *index.entry(text.clone()).or_insert_with(|| {
            out.entries.push(Entry { name: shown, sid: text, allow: 0, deny: 0 });
            out.entries.len() - 1
        });
        if ty == ACCESS_ALLOWED_ACE_TYPE {
            out.entries[at].allow |= mask;
        } else {
            out.entries[at].deny |= mask;
        }
    }
    out
}

/// The standard permission rows of Explorer's Security tab, in its order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Right {
    Full,
    Modify,
    ReadExecute,
    /// Folders only.
    List,
    Read,
    Write,
    Special,
}

pub const RIGHTS: [Right; 7] = [Right::Full, Right::Modify, Right::ReadExecute, Right::List, Right::Read, Right::Write, Right::Special];

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const GENERIC_EXECUTE: u32 = 0x2000_0000;
const GENERIC_ALL: u32 = 0x1000_0000;
const FILE_GENERIC_READ: u32 = 0x0012_0089;
const FILE_GENERIC_WRITE: u32 = 0x0012_0116;
const FILE_GENERIC_EXECUTE: u32 = 0x0012_00A0;
const FILE_ALL_ACCESS: u32 = 0x001F_01FF;
const DELETE: u32 = 0x0001_0000;
const MODIFY: u32 = FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE;

/// GENERIC_* bits turned into the file-specific rights they stand for.
fn map_generic(mask: u32) -> u32 {
    let mut m = mask & !(GENERIC_READ | GENERIC_WRITE | GENERIC_EXECUTE | GENERIC_ALL);
    if mask & GENERIC_READ != 0 {
        m |= FILE_GENERIC_READ;
    }
    if mask & GENERIC_WRITE != 0 {
        m |= FILE_GENERIC_WRITE;
    }
    if mask & GENERIC_EXECUTE != 0 {
        m |= FILE_GENERIC_EXECUTE;
    }
    if mask & GENERIC_ALL != 0 {
        m |= FILE_ALL_ACCESS;
    }
    m
}

/// Which rows a mask ticks, the way Explorer decides it, in `RIGHTS` order. Special means bits
/// remain that no ticked standard row explains.
pub fn ticks(mask: u32, is_dir: bool) -> Vec<Right> {
    let m = map_generic(mask);
    let has = |bits: u32| m & bits == bits;
    let mut out = Vec::new();
    let mut covered = 0u32;
    macro_rules! add {
        ($r:expr, $bits:expr) => {{
            out.push($r);
            covered |= $bits;
        }};
    }
    if has(FILE_ALL_ACCESS) {
        add!(Right::Full, FILE_ALL_ACCESS);
    }
    if has(MODIFY) {
        add!(Right::Modify, MODIFY);
    }
    if has(FILE_GENERIC_READ | FILE_GENERIC_EXECUTE) {
        add!(Right::ReadExecute, FILE_GENERIC_READ | FILE_GENERIC_EXECUTE);
        if is_dir {
            out.push(Right::List);
        }
    }
    if has(FILE_GENERIC_READ) {
        add!(Right::Read, FILE_GENERIC_READ);
    }
    if has(FILE_GENERIC_WRITE) {
        add!(Right::Write, FILE_GENERIC_WRITE);
    }
    if m & !covered != 0 {
        out.push(Right::Special);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truth_table() {
        let all = vec![Right::Full, Right::Modify, Right::ReadExecute, Right::Read, Right::Write];
        assert_eq!(ticks(0x1F01FF, false), all);
        assert_eq!(ticks(0x1F01FF, true), vec![Right::Full, Right::Modify, Right::ReadExecute, Right::List, Right::Read, Right::Write]);
        assert_eq!(ticks(0x1301BF, false), vec![Right::Modify, Right::ReadExecute, Right::Read, Right::Write]);
        assert_eq!(ticks(0x1200A9, false), vec![Right::ReadExecute, Right::Read]);
        assert_eq!(ticks(0x1200A9, true), vec![Right::ReadExecute, Right::List, Right::Read]);
        assert_eq!(ticks(0x120089, false), vec![Right::Read]);
        assert_eq!(ticks(0x120116, false), vec![Right::Write]);
        assert_eq!(ticks(0, false), vec![]);
    }

    #[test]
    fn generic_bits_are_mapped() {
        assert_eq!(ticks(GENERIC_ALL, false), ticks(FILE_ALL_ACCESS, false));
        assert_eq!(ticks(GENERIC_READ, false), vec![Right::Read]);
        assert_eq!(ticks(GENERIC_READ | GENERIC_EXECUTE, true), vec![Right::ReadExecute, Right::List, Right::Read]);
        assert_eq!(ticks(GENERIC_WRITE, false), vec![Right::Write]);
    }

    #[test]
    fn leftover_bits_are_special() {
        // WRITE_DAC alone
        assert_eq!(ticks(0x40000, false), vec![Right::Special]);
        // read plus take-ownership
        assert_eq!(ticks(0x120089 | 0x80000, false), vec![Right::Read, Right::Special]);
        // delete child on a folder with modify rights
        assert_eq!(ticks(0x1301BF | 0x40, true), vec![Right::Modify, Right::ReadExecute, Right::List, Right::Read, Right::Write, Right::Special]);
    }

    #[test]
    fn reads_a_temp_file() {
        let dir = std::env::temp_dir().join(format!("rf-sec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("x.txt");
        std::fs::write(&f, b"hi").unwrap();
        let s = read(&f).expect("own temp file is readable");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(s.owner.is_some(), "{s:?}");
        assert!(!s.entries.is_empty(), "{s:?}");
        let user = std::env::var("USERNAME").unwrap_or_default().to_lowercase();
        assert!(
            s.entries.iter().any(|e| {
                let n = e.name.to_lowercase();
                n.contains("administrators") || n.contains("system") || (!user.is_empty() && n.contains(&user))
            }),
            "{s:?}"
        );
        assert!(s.entries.iter().all(|e| e.sid.starts_with("S-1-")));
    }

    #[test]
    fn missing_file_is_an_error() {
        assert!(read(Path::new(r"C:\definitely\not\here.txt")).is_err());
    }
}
