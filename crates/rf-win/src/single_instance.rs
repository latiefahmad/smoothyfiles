//! Single-instance plumbing: the first process owns a named mutex and serves a named pipe;
//! later launches forward their arguments (paths) through the pipe and exit.

use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GENERIC_WRITE, GetLastError, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_NONE, OPEN_EXISTING, PIPE_ACCESS_INBOUND, ReadFile, WriteFile,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES,
    PIPE_WAIT,
};
use windows::Win32::System::Threading::CreateMutexW;
use windows::core::PCWSTR;

use crate::wide::wide;

/// (kilit adı, boru adı) bir uygulama için: smoothyfiles ve DirectLook ayrı örnek gruplarıdır.
fn names_for(app: &str) -> (String, String) {
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
    // A separate settings folder (tests, portable copies) is a separate instance group:
    // its second launch must not land in the user's everyday window.
    let suffix = match std::env::var_os("SMOOTHYFILES_CONFIG_DIR") {
        Some(dir) => {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            dir.to_string_lossy().to_lowercase().hash(&mut h);
            format!("-{:08x}", h.finish() as u32)
        }
        None => String::new(),
    };
    (format!("Local\\{app}-{user}{suffix}"), format!(r"\\.\pipe\{app}-{user}{suffix}"))
}

pub enum Role {
    /// We are the first instance. Keep the guard alive for the process lifetime.
    Primary(InstanceGuard),
    /// Another instance is running and received our message.
    Forwarded,
}

pub struct InstanceGuard(HANDLE);
unsafe impl Send for InstanceGuard {}
impl Drop for InstanceGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Decides the role. `message` is sent to the primary when one exists ("" = just focus a new window/tab).
pub fn acquire(message: &str) -> Role {
    acquire_app("smoothyfiles", message)
}

/// [`acquire`] for another application of the bundle (e.g. "directlook").
pub fn acquire_app(app: &str, message: &str) -> Role {
    let (mutex_name, pipe_name) = names_for(app);
    let w = wide(&mutex_name);
    let handle = unsafe { CreateMutexW(None, false, PCWSTR(w.as_ptr())) };
    let exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    match handle {
        Ok(h) if !exists => Role::Primary(InstanceGuard(h)),
        Ok(h) => {
            // Primary may still be starting its pipe: retry briefly.
            for _ in 0..40 {
                if send(&pipe_name, message) {
                    unsafe {
                        let _ = CloseHandle(h);
                    }
                    return Role::Forwarded;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            // Primary unresponsive (hung or exiting): run on our own.
            Role::Primary(InstanceGuard(h))
        }
        Err(_) => Role::Primary(InstanceGuard(HANDLE::default())),
    }
}

/// Sends `message` to the running instance of `app` (false if none answers). Never waits long:
/// the pipe either exists right now or it doesn't.
pub fn send_to(app: &str, message: &str) -> bool {
    send(&names_for(app).1, message)
}

fn send(pipe: &str, message: &str) -> bool {
    let w = wide(pipe);
    unsafe {
        let Ok(h) = CreateFileW(
            PCWSTR(w.as_ptr()),
            GENERIC_WRITE.0,
            FILE_SHARE_NONE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        ) else {
            return false;
        };
        let mut bytes = message.as_bytes().to_vec();
        bytes.push(b'\n');
        let ok = WriteFile(h, Some(&bytes), None, None).is_ok();
        let _ = CloseHandle(h);
        ok
    }
}

/// Serves the pipe on a background thread; each received line is passed to `on_message`.
pub fn serve(on_message: impl Fn(String) + Send + Sync + 'static) {
    serve_app("smoothyfiles", on_message)
}

/// [`serve`] for another application of the bundle.
pub fn serve_app(app: &str, on_message: impl Fn(String) + Send + Sync + 'static) {
    let on_message = std::sync::Arc::new(on_message);
    let (_, pipe_name) = names_for(app);
    std::thread::Builder::new()
        .name("rf-instance-pipe".into())
        .spawn(move || {
            let w = wide(&pipe_name);
            loop {
                let pipe = unsafe {
                    CreateNamedPipeW(
                        PCWSTR(w.as_ptr()),
                        PIPE_ACCESS_INBOUND,
                        PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                        PIPE_UNLIMITED_INSTANCES,
                        0,
                        64 * 1024,
                        0,
                        None,
                    )
                };
                if pipe.is_invalid() {
                    log::warn!("single instance pipe failed");
                    return;
                }
                unsafe {
                    let _ = ConnectNamedPipe(pipe, None);
                }
                // Read each client on its own thread so a stalled client can't block the next one.
                let raw = pipe.0 as isize;
                let cb = on_message.clone();
                std::thread::spawn(move || unsafe {
                    let pipe = HANDLE(raw as *mut _);
                    let mut buf = vec![0u8; 16 * 1024];
                    let mut total = Vec::new();
                    loop {
                        let mut n = 0u32;
                        if ReadFile(pipe, Some(&mut buf), Some(&mut n), None).is_err() || n == 0 {
                            break;
                        }
                        total.extend_from_slice(&buf[..n as usize]);
                        if total.len() > 64 * 1024 {
                            break;
                        }
                    }
                    let _ = DisconnectNamedPipe(pipe);
                    let _ = CloseHandle(pipe);
                    for line in String::from_utf8_lossy(&total).lines() {
                        cb(line.to_string());
                    }
                });
            }
        })
        .ok();
}

#[cfg(test)]
mod app_tests {
    use super::*;

    #[test]
    fn apps_have_separate_names() {
        let (m1, p1) = names_for("smoothyfiles");
        let (m2, p2) = names_for("directlook");
        assert_ne!(m1, m2);
        assert_ne!(p1, p2);
        assert!(p2.contains("directlook"));
    }

    #[test]
    fn send_reaches_a_serving_app_and_fails_without_one() {
        let app = format!("rf-test-{}", std::process::id());
        assert!(!send_to(&app, "yok"));
        let (tx, rx) = std::sync::mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        serve_app(&app, move |m| {
            let _ = tx.lock().unwrap().send(m);
        });
        let t = std::time::Instant::now();
        while !send_to(&app, "merhaba") {
            assert!(t.elapsed().as_secs() < 3, "pipe never came up");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(3)).unwrap(), "merhaba");
    }
}
