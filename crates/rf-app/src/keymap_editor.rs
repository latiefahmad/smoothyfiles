//! Shortcut editor (Settings → Keyboard shortcuts): click a command, press the new keys.
//!
//! Capturing uses a keystroke interceptor, so the pressed keys never reach their current action
//! (pressing Ctrl+N while recording does not open a window). Escape cancels. The new binding is
//! stored in `settings.toml` → `[keybindings]` and bound immediately; bindings are additive, so
//! the previous keys keep working until restart after a reset.

use gpui_kit::*;

/// The command (short action name, e.g. "NewFolder") waiting for keys and when recording started (a forgotten capture expires).
#[derive(Default)]
pub struct KeyCapture(pub Option<(String, std::time::Instant)>);
impl Global for KeyCapture {}

const MODIFIER_KEYS: [&str; 6] = ["control", "shift", "alt", "platform", "function", ""];

/// GPUI keystroke string (`ctrl-shift-k`) from a keystroke, or `None` for a modifier-only press.
fn keys_of(k: &Keystroke) -> Option<String> {
    if MODIFIER_KEYS.contains(&k.key.as_str()) {
        return None;
    }
    let m = &k.modifiers;
    let mut s = String::new();
    if m.control {
        s.push_str("ctrl-");
    }
    if m.alt {
        s.push_str("alt-");
    }
    if m.shift {
        s.push_str("shift-");
    }
    if m.platform {
        s.push_str("win-");
    }
    s.push_str(&k.key);
    Some(s)
}

/// "ctrl-shift-k" → "Ctrl+Shift+K" (as shown in the UI).
pub fn pretty(keys: &str) -> String {
    // "ctrl--" means Ctrl + minus: keep the trailing '-' as the key
    if let Some(mods) = keys.strip_suffix("--") {
        let head = pretty(mods);
        return if head.is_empty() { "-".into() } else { format!("{head}+-") };
    }
    keys.split('-')
        .map(|p| match p {
            "ctrl" => "Ctrl".to_string(),
            "alt" => "Alt".to_string(),
            "shift" => "Shift".to_string(),
            "win" => "Win".to_string(),
            k if k.chars().count() == 1 => k.to_uppercase(),
            k => {
                let mut c = k.chars();
                c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
            }
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// Installs the interceptor once at startup.
pub fn init(cx: &mut App) {
    cx.set_global(KeyCapture::default());
    cx.intercept_keystrokes(|ev, window, cx| {
        let Some((action, started)) = cx.global::<KeyCapture>().0.clone() else { return };
        let quick_search = action == crate::quick_search::CAPTURE_ACTION;
        if started.elapsed() > std::time::Duration::from_secs(15) {
            // left Settings without pressing anything: give the keyboard back
            cx.global_mut::<KeyCapture>().0 = None;
            if quick_search {
                crate::quick_search::resync(cx);
            }
            return;
        }
        cx.stop_propagation();
        if ev.keystroke.key == "escape" && !ev.keystroke.modifiers.modified() {
            cx.global_mut::<KeyCapture>().0 = None;
            if quick_search {
                crate::quick_search::resync(cx);
            }
            window.refresh();
            return;
        }
        let Some(keys) = keys_of(&ev.keystroke) else { return };
        cx.global_mut::<KeyCapture>().0 = None;
        if quick_search {
            // the quick search's global shortcut: a setting, not an action binding
            crate::settings::update(cx, |s| s.quick_search_keys = keys.clone());
            crate::quick_search::resync(cx);
            window.refresh();
            return;
        }
        crate::settings::update(cx, |s| {
            // one custom shortcut per command: replace an earlier one
            s.keybindings.retain(|_, a| *a != action);
            s.keybindings.insert(keys.clone(), action.clone());
        });
        // bind just the new key (re-binding the whole map would stack duplicates)
        match cx.build_action(&format!("smoothyfiles::{action}"), None) {
            Ok(a) => match KeyBinding::load(&keys, a, None, false, None, cx) {
                Ok(b) => cx.bind_keys([b]),
                Err(e) => log::warn!("keybinding {keys:?}: {e}"),
            },
            Err(e) => log::warn!("keybinding action {action:?}: {e}"),
        }
        window.refresh();
    })
    .detach();
}

pub fn start(action: String, cx: &mut App) {
    let quick_search = action == crate::quick_search::CAPTURE_ACTION;
    // recording the quick search's shortcut was given up for another command: it comes back
    if !quick_search && capturing(cx).as_deref() == Some(crate::quick_search::CAPTURE_ACTION) {
        crate::quick_search::resync(cx);
    }
    let started = std::time::Instant::now();
    if quick_search {
        // the current global shortcut would swallow the keys being recorded
        crate::quick_search::release_for_capture();
        // left without pressing anything (the window closed, another app came up): the
        // shortcut comes back by itself, it would otherwise stay off until a key reaches us
        cx.spawn(async move |cx| {
            cx.background_executor().timer(std::time::Duration::from_secs(16)).await;
            cx.update(|cx| {
                let still = cx.global::<KeyCapture>().0.as_ref().is_some_and(|(a, t)| a == crate::quick_search::CAPTURE_ACTION && *t == started);
                if still {
                    cx.global_mut::<KeyCapture>().0 = None;
                    crate::quick_search::resync(cx);
                    cx.refresh_windows();
                }
            });
        })
        .detach();
    }
    cx.global_mut::<KeyCapture>().0 = Some((action, started));
}

pub fn capturing(cx: &App) -> Option<String> {
    cx.try_global::<KeyCapture>().and_then(|c| c.0.as_ref().map(|(a, _)| a.clone()))
}

/// User override for a command, if any (keys in GPUI syntax).
pub fn custom_keys(action: &str, cx: &App) -> Option<String> {
    cx.global::<crate::settings::Settings>()
        .keybindings
        .iter()
        .find(|(_, a)| a.as_str() == action || a.strip_prefix("smoothyfiles::") == Some(action))
        .map(|(k, _)| k.clone())
}

pub fn reset(action: &str, cx: &mut App) {
    let action = action.to_string();
    crate::settings::update(cx, |s| {
        s.keybindings.retain(|_, a| *a != action && a.strip_prefix("smoothyfiles::") != Some(action.as_str()))
    });
}

#[cfg(test)]
mod tests {
    use super::pretty;

    #[test]
    fn pretty_keys() {
        assert_eq!(pretty("ctrl-shift-k"), "Ctrl+Shift+K");
        assert_eq!(pretty("alt-f4"), "Alt+F4");
        assert_eq!(pretty("ctrl-pageup"), "Ctrl+Pageup");
        assert_eq!(pretty("ctrl--"), "Ctrl+-");
    }
}
