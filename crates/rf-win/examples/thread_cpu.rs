//! Per-thread CPU time of a process, with thread names:
//! `cargo run --release -p rf-win --example thread_cpu -- <pid>`.

use windows::Win32::Foundation::{CloseHandle, FILETIME};
use windows::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next};
use windows::Win32::System::Threading::{GetThreadDescription, GetThreadTimes, OpenThread, THREAD_QUERY_LIMITED_INFORMATION};

/// (base, size, name) of every module loaded in the process.
fn modules(pid: u32) -> Vec<(usize, usize, String)> {
    use windows::Win32::System::ProcessStatus::{EnumProcessModulesEx, GetModuleBaseNameW, GetModuleInformation, LIST_MODULES_ALL, MODULEINFO};
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ};
    let mut out = Vec::new();
    unsafe {
        let Ok(p) = OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, pid) else { return out };
        let mut mods = vec![windows::Win32::Foundation::HMODULE::default(); 1024];
        let mut need = 0u32;
        if EnumProcessModulesEx(p, mods.as_mut_ptr(), (mods.len() * 8) as u32, &mut need, LIST_MODULES_ALL).is_ok() {
            for m in mods.iter().take(need as usize / 8) {
                let mut info = MODULEINFO::default();
                let _ = GetModuleInformation(p, *m, &mut info, std::mem::size_of::<MODULEINFO>() as u32);
                let mut name = [0u16; 260];
                let n = GetModuleBaseNameW(p, Some(*m), &mut name);
                out.push((info.lpBaseOfDll as usize, info.SizeOfImage as usize, String::from_utf16_lossy(&name[..n as usize])));
            }
        }
        let _ = CloseHandle(p);
    }
    out
}

/// The thread's start address (NtQueryInformationThread, ThreadQuerySetWin32StartAddress).
fn start_address(h: windows::Win32::Foundation::HANDLE) -> usize {
    use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    type Nqit = unsafe extern "system" fn(windows::Win32::Foundation::HANDLE, u32, *mut usize, u32, *mut u32) -> i32;
    unsafe {
        let Ok(nt) = GetModuleHandleW(windows::core::w!("ntdll.dll")) else { return 0 };
        let Some(f) = GetProcAddress(nt, windows::core::s!("NtQueryInformationThread")) else { return 0 };
        let f: Nqit = std::mem::transmute(f);
        let mut a = 0usize;
        f(h, 9, &mut a, 8, std::ptr::null_mut());
        a
    }
}

fn main() {
    let pid: u32 = std::env::args().nth(1).expect("pid").parse().unwrap();
    let mods = modules(pid);
    let mut rows = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0).unwrap();
        let mut e = THREADENTRY32 { dwSize: std::mem::size_of::<THREADENTRY32>() as u32, ..Default::default() };
        let mut ok = Thread32First(snap, &mut e).is_ok();
        while ok {
            if e.th32OwnerProcessID == pid {
                if let Ok(h) = OpenThread(THREAD_QUERY_LIMITED_INFORMATION | windows::Win32::System::Threading::THREAD_QUERY_INFORMATION, false, e.th32ThreadID) {
                    let (mut c, mut x, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
                    let _ = GetThreadTimes(h, &mut c, &mut x, &mut k, &mut u);
                    let t = |f: FILETIME| (((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64) as f64 / 10_000.0;
                    let mut name = GetThreadDescription(h).ok().and_then(|p| p.to_string().ok()).unwrap_or_default();
                    let a = start_address(h);
                    if let Some(m) = mods.iter().find(|m| a >= m.0 && a < m.0 + m.1) {
                        name = format!("{name} [{}]", m.2);
                    }
                    rows.push((t(k) + t(u), e.th32ThreadID, name));
                    let _ = CloseHandle(h);
                }
            }
            ok = Thread32Next(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    rows.sort_by(|a, b| b.0.total_cmp(&a.0));
    let total: f64 = rows.iter().map(|r| r.0).sum();
    println!("{} threads, {total:.0} ms CPU", rows.len());
    for (ms, id, name) in rows.iter().take(25) {
        println!("{ms:>8.0} ms  {id:>6}  {name}");
    }
}
