//! `SMOOTHYFILES_LOOK_TRACE=1`: gösterimden itibaren geçen süreyi stderr'e yazar (her olay bir kez).

use std::sync::Mutex;
use std::time::Instant;

static STATE: Mutex<Option<(Instant, Vec<&'static str>)>> = Mutex::new(None);

fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("SMOOTHYFILES_LOOK_TRACE").is_some())
}

/// Yeni gösterim başladı.
pub fn start() {
    if on() {
        *STATE.lock().unwrap_or_else(|e| e.into_inner()) = Some((Instant::now(), Vec::new()));
    }
}

/// Olay (bu gösterimde ilk kez görüldüyse yazılır).
pub fn mark(what: &'static str) {
    if !on() {
        return;
    }
    let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((t, seen)) = g.as_mut() {
        if !seen.contains(&what) {
            seen.push(what);
            eprintln!("look: {what} {:.1} ms", t.elapsed().as_secs_f64() * 1000.0);
        }
    }
}
