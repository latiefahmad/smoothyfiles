//! Program compatibility settings (AppCompatFlags layers, per user), for the Properties window's Compatibility tab.
//! The per-user layers live in `HKCU\Software\Microsoft\Windows NT\CurrentVersion\AppCompatFlags\Layers`: one
//! REG_SZ per program, named by the exe's full path, holding space-separated layer names. We only ever write
//! HKCU (no elevation); the all-users copy under HKLM is read so the tab can show it as already applied.

use std::path::Path;

use windows_registry::{CURRENT_USER, Key, LOCAL_MACHINE};

const LAYERS: &str = r"Software\Microsoft\Windows NT\CurrentVersion\AppCompatFlags\Layers";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Win95,
    Win98,
    WinXpSp2,
    WinXpSp3,
    Vista,
    VistaSp1,
    VistaSp2,
    Win7,
    Win8,
}

impl Mode {
    pub const ALL: [Mode; 9] = [Mode::Win95, Mode::Win98, Mode::WinXpSp2, Mode::WinXpSp3, Mode::Vista, Mode::VistaSp1, Mode::VistaSp2, Mode::Win7, Mode::Win8];

    /// The layer name Windows stores.
    pub fn layer(self) -> &'static str {
        match self {
            Mode::Win95 => "WIN95",
            Mode::Win98 => "WIN98",
            Mode::WinXpSp2 => "WINXPSP2",
            Mode::WinXpSp3 => "WINXPSP3",
            Mode::Vista => "VISTARTM",
            Mode::VistaSp1 => "VISTASP1",
            Mode::VistaSp2 => "VISTASP2",
            Mode::Win7 => "WIN7RTM",
            Mode::Win8 => "WIN8RTM",
        }
    }

    /// The name the Compatibility tab shows.
    pub fn label(self) -> &'static str {
        match self {
            Mode::Win95 => "Windows 95",
            Mode::Win98 => "Windows 98 / Windows ME",
            Mode::WinXpSp2 => "Windows XP (Service Pack 2)",
            Mode::WinXpSp3 => "Windows XP (Service Pack 3)",
            Mode::Vista => "Windows Vista",
            Mode::VistaSp1 => "Windows Vista (Service Pack 1)",
            Mode::VistaSp2 => "Windows Vista (Service Pack 2)",
            Mode::Win7 => "Windows 7",
            Mode::Win8 => "Windows 8",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    /// "256COLOR"
    C256,
    /// "16BITCOLOR"
    C16Bit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dpi {
    /// "HIGHDPIAWARE"
    Application,
    /// "DPIUNAWARE"
    System,
    /// "GDIDPISCALING DPIUNAWARE"
    SystemEnhanced,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Compat {
    pub mode: Option<Mode>,
    pub color: Option<Color>,
    /// "640X480"
    pub res_640: bool,
    /// "DISABLEDXMAXIMIZEDWINDOWEDMODE"
    pub disable_fso: bool,
    /// "RUNASADMIN"
    pub run_as_admin: bool,
    /// "REGISTERAPPRESTART"
    pub register_restart: bool,
    /// "TRANSFORMLEGACYCOLORMANAGED"
    pub legacy_icc: bool,
    pub dpi: Option<Dpi>,
    /// Layers we show no control for, kept as they were on write.
    pub other: Vec<String>,
}

/// Reads a layers string such as `"~ RUNASADMIN WIN7RTM HIGHDPIAWARE"`. Tolerant of the `~`, `#` and `$`
/// markers Windows puts in front, of any order, and of letter case.
pub fn parse(data: &str) -> Compat {
    let mut c = Compat::default();
    let (mut gdi, mut unaware, mut aware) = (false, false, false);
    for tok in data.split_whitespace() {
        if matches!(tok, "~" | "#" | "$") {
            continue;
        }
        let up = tok.to_ascii_uppercase();
        if let Some(m) = Mode::ALL.iter().find(|m| m.layer() == up) {
            c.mode = Some(*m);
            continue;
        }
        match up.as_str() {
            "256COLOR" => c.color = Some(Color::C256),
            "16BITCOLOR" => c.color = Some(Color::C16Bit),
            "640X480" => c.res_640 = true,
            "DISABLEDXMAXIMIZEDWINDOWEDMODE" => c.disable_fso = true,
            "RUNASADMIN" => c.run_as_admin = true,
            "REGISTERAPPRESTART" => c.register_restart = true,
            "TRANSFORMLEGACYCOLORMANAGED" => c.legacy_icc = true,
            "HIGHDPIAWARE" => aware = true,
            "DPIUNAWARE" => unaware = true,
            "GDIDPISCALING" => gdi = true,
            _ => c.other.push(tok.to_string()),
        }
    }
    c.dpi = if aware {
        Some(Dpi::Application)
    } else if unaware && gdi {
        Some(Dpi::SystemEnhanced)
    } else if unaware {
        Some(Dpi::System)
    } else {
        None
    };
    if gdi && !unaware {
        // GDIDPISCALING without DPIUNAWARE is not a combination we offer: keep it untouched
        c.other.push("GDIDPISCALING".to_string());
    }
    c
}

/// The layers string to store: `"~ "` plus the layers, or `""` when nothing is set.
pub fn format(c: &Compat) -> String {
    let mut v: Vec<&str> = Vec::new();
    if let Some(m) = c.mode {
        v.push(m.layer());
    }
    match c.color {
        Some(Color::C256) => v.push("256COLOR"),
        Some(Color::C16Bit) => v.push("16BITCOLOR"),
        None => {}
    }
    if c.res_640 {
        v.push("640X480");
    }
    match c.dpi {
        Some(Dpi::Application) => v.push("HIGHDPIAWARE"),
        Some(Dpi::System) => v.push("DPIUNAWARE"),
        Some(Dpi::SystemEnhanced) => {
            v.push("GDIDPISCALING");
            v.push("DPIUNAWARE");
        }
        None => {}
    }
    if c.disable_fso {
        v.push("DISABLEDXMAXIMIZEDWINDOWEDMODE");
    }
    if c.run_as_admin {
        v.push("RUNASADMIN");
    }
    if c.register_restart {
        v.push("REGISTERAPPRESTART");
    }
    if c.legacy_icc {
        v.push("TRANSFORMLEGACYCOLORMANAGED");
    }
    v.extend(c.other.iter().map(String::as_str));
    if v.is_empty() { String::new() } else { format!("~ {}", v.join(" ")) }
}

fn read_from(root: &Key, exe: &Path) -> Compat {
    root.open(LAYERS).and_then(|k| k.get_string(exe.to_string_lossy().as_ref())).map(|s| parse(&s)).unwrap_or_default()
}

/// This user's settings for the program (HKCU).
pub fn read(exe: &Path) -> Compat {
    read_from(CURRENT_USER, exe)
}

/// The all-users settings for the program (HKLM), read-only.
pub fn read_all_users(exe: &Path) -> Compat {
    read_from(LOCAL_MACHINE, exe)
}

/// Stores the settings for this user (HKCU only); removes the value when nothing is set.
pub fn write(exe: &Path, c: &Compat) -> anyhow::Result<()> {
    let name = exe.to_string_lossy().to_string();
    let text = format(c);
    if text.is_empty() {
        if let Ok(k) = CURRENT_USER.options().write().open(LAYERS) {
            // already absent is fine
            let _ = k.remove_value(&name);
        }
        return Ok(());
    }
    CURRENT_USER.create(LAYERS)?.set_string(&name, &text)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_format_roundtrip() {
        let c = parse("~ RUNASADMIN WIN7RTM HIGHDPIAWARE");
        assert_eq!(c.mode, Some(Mode::Win7));
        assert!(c.run_as_admin);
        assert_eq!(c.dpi, Some(Dpi::Application));
        assert_eq!(format(&c), "~ WIN7RTM HIGHDPIAWARE RUNASADMIN");
        assert_eq!(parse(&format(&c)), c);

        let c = parse("# 256COLOR 640X480 GDIDPISCALING DPIUNAWARE DISABLEDXMAXIMIZEDWINDOWEDMODE");
        assert_eq!(c.color, Some(Color::C256));
        assert!(c.res_640 && c.disable_fso);
        assert_eq!(c.dpi, Some(Dpi::SystemEnhanced));
        assert_eq!(parse(&format(&c)), c);

        assert_eq!(parse("DPIUNAWARE").dpi, Some(Dpi::System));
        assert_eq!(format(&Compat::default()), "");
        assert_eq!(format(&parse("~")), "");
        for m in Mode::ALL {
            assert_eq!(parse(m.layer()).mode, Some(m));
        }
    }

    #[test]
    fn unknown_layers_are_kept() {
        let c = parse("~ WINXPSP3 SOMENEWLAYER 16BITCOLOR REGISTERAPPRESTART TRANSFORMLEGACYCOLORMANAGED");
        assert_eq!(c.other, vec!["SOMENEWLAYER".to_string()]);
        assert!(c.register_restart && c.legacy_icc);
        assert_eq!(c.color, Some(Color::C16Bit));
        assert!(format(&c).contains("SOMENEWLAYER"));
        assert_eq!(parse(&format(&c)), c);
    }

    #[test]
    fn write_read_delete_on_a_fake_path() {
        let exe = std::env::temp_dir().join(format!("rf-compat-test-{}.exe", std::process::id()));
        assert_eq!(read(&exe), Compat::default());
        let c = Compat { mode: Some(Mode::Win8), run_as_admin: true, dpi: Some(Dpi::System), ..Default::default() };
        write(&exe, &c).unwrap();
        let back = read(&exe);
        assert_eq!(back, c);
        write(&exe, &Compat::default()).unwrap();
        assert_eq!(read(&exe), Compat::default());
        let still = CURRENT_USER.open(LAYERS).and_then(|k| k.get_string(exe.to_string_lossy().as_ref())).is_ok();
        assert!(!still);
        write(&exe, &Compat::default()).unwrap(); // nothing to remove: fine
    }
}
