//! smoothyfiles ↔ DirectLook iletileri (tek örnek borusu üzerinden, satır başına bir ileti).

use std::path::PathBuf;

use super::ShowOpts;

/// DirectLook'un tek örnek adı (boru ve kilit).
pub const APP: &str = "directlook";

/// The single-instance name. `SMOOTHYFILES_LOOK_INSTANCE=<x>` gives a test copy its own
/// ("directlook-<x>"), so it never talks to the user's running DirectLook.
pub fn app_name() -> String {
    match std::env::var("SMOOTHYFILES_LOOK_INSTANCE") {
        Ok(x) if !x.is_empty() => format!("{APP}-{x}"),
        _ => APP.to_string(),
    }
}
const PREFIX: &str = "\u{1}dl:";
const TO_APP_PREFIX: &str = "\u{1}directlook:";

/// DirectLook'a giden ileti.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Msg {
    /// Göster (komut satırı `--look`).
    Show(PathBuf, ShowOpts),
    /// Açıksa kapat, değilse göster (smoothyfiles'ta Boşluk).
    Toggle(PathBuf),
    /// Seçim değişti: açıksa bu dosyaya geç, değilse bir şey yapma.
    Follow(PathBuf),
    Close,
    /// Ayar dosyası değişti.
    Reload,
    Quit,
    /// Yalnız çalışsın (arka planda başlatma).
    Wake,
}

impl Msg {
    pub fn encode(&self) -> String {
        let opts = |o: &ShowOpts| [(o.topmost, "top"), (o.pin, "pin")].iter().filter(|(on, _)| *on).map(|(_, n)| *n).collect::<Vec<_>>().join(",");
        match self {
            Msg::Show(p, o) => format!("{PREFIX}show|{}|{}", opts(o), p.display()),
            Msg::Toggle(p) => format!("{PREFIX}toggle|{}", p.display()),
            Msg::Follow(p) => format!("{PREFIX}follow|{}", p.display()),
            Msg::Close => format!("{PREFIX}close"),
            Msg::Reload => format!("{PREFIX}reload"),
            Msg::Quit => format!("{PREFIX}quit"),
            Msg::Wake => String::new(),
        }
    }

    pub fn decode(s: &str) -> Option<Msg> {
        if s.trim().is_empty() {
            return Some(Msg::Wake);
        }
        let rest = s.strip_prefix(PREFIX)?;
        let (verb, arg) = rest.split_once('|').unwrap_or((rest, ""));
        Some(match verb {
            "show" => {
                let (opts, path) = arg.split_once('|')?;
                let mut o = ShowOpts::default();
                for w in opts.split(',') {
                    match w {
                        "top" => o.topmost = true,
                        "pin" => o.pin = true,
                        _ => {}
                    }
                }
                Msg::Show(PathBuf::from(path), o)
            }
            "toggle" => Msg::Toggle(PathBuf::from(arg)),
            "follow" => Msg::Follow(PathBuf::from(arg)),
            "close" => Msg::Close,
            "reload" => Msg::Reload,
            "quit" => Msg::Quit,
            _ => return None,
        })
    }
}

/// DirectLook'tan smoothyfiles'a giden ileti.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToApp {
    /// Önizleme penceresi kapandı (smoothyfiles "açık" bilgisini bırakır).
    LookClosed,
    /// Ayarlar → DirectLook sayfasını aç.
    OpenSettings,
}

impl ToApp {
    pub fn encode(&self) -> String {
        match self {
            ToApp::LookClosed => format!("{TO_APP_PREFIX}closed"),
            ToApp::OpenSettings => format!("{TO_APP_PREFIX}settings"),
        }
    }

    pub fn decode(s: &str) -> Option<ToApp> {
        match s.strip_prefix(TO_APP_PREFIX)? {
            "closed" => Some(ToApp::LookClosed),
            "settings" => Some(ToApp::OpenSettings),
            _ => None,
        }
    }
}

/// directlook.exe: bu programın yanında.
pub fn exe() -> Option<PathBuf> {
    let me = std::env::current_exe().ok()?;
    let p = me.with_file_name("directlook.exe");
    p.exists().then_some(p)
}

/// İletiyi DirectLook'a gönderir; çalışmıyorsa (ve `start` ise) onu bu iletiyle başlatır.
/// Hiç beklemez: arka planda yapılır.
pub fn send(msg: Msg, start: bool) {
    std::thread::spawn(move || {
        let text = msg.encode();
        if rf_win::single_instance::send_to(&app_name(), &text) {
            return;
        }
        if !start {
            return;
        }
        let Some(exe) = exe() else {
            log::warn!("directlook.exe bulunamadı");
            return;
        };
        let mut cmd = std::process::Command::new(exe);
        if text.is_empty() {
            cmd.arg("--background");
        } else {
            cmd.args(["--msg", &text]);
        }
        if let Err(e) = cmd.spawn() {
            log::warn!("directlook başlatılamadı: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{Msg, ToApp};
    use crate::look::ShowOpts;

    #[test]
    fn messages_round_trip() {
        let p = PathBuf::from(r"C:\a b\ş|x.mp4");
        for m in [
            Msg::Show(p.clone(), ShowOpts { topmost: true, pin: false }),
            Msg::Toggle(p.clone()),
            Msg::Follow(p.clone()),
            Msg::Close,
            Msg::Reload,
            Msg::Quit,
            Msg::Wake,
        ] {
            assert_eq!(Msg::decode(&m.encode()), Some(m.clone()), "{m:?}");
        }
        assert_eq!(Msg::decode(r"C:\klasör"), None);
        assert_eq!(Msg::decode(""), Some(Msg::Wake));
    }

    #[test]
    fn messages_to_smoothyfiles() {
        for m in [ToApp::LookClosed, ToApp::OpenSettings] {
            assert_eq!(ToApp::decode(&m.encode()), Some(m));
        }
        assert_eq!(ToApp::decode(r"C:\Windows"), None);
    }
}
