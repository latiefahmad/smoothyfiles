//! The user's Windows accent color palette (Settings → Personalization → Colors), so the app
//! wears the same color as the taskbar, Start and Files.

/// Shades of the accent, as 0xRRGGBB: `[light3, light2, light1, base, dark1, dark2, dark3]`.
/// Fluent uses `light2` for accents on dark surfaces and `dark1` on light ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccentPalette {
    pub shades: [u32; 7],
}

impl AccentPalette {
    pub fn light2(&self) -> u32 {
        self.shades[1]
    }
    pub fn dark1(&self) -> u32 {
        self.shades[4]
    }
    pub fn base(&self) -> u32 {
        self.shades[3]
    }
}

/// `AccentPalette` is 8 RGBA quadruples; the first seven are the shades.
pub fn parse_palette(raw: &[u8]) -> Option<AccentPalette> {
    if raw.len() < 28 {
        return None;
    }
    let mut shades = [0u32; 7];
    for (i, s) in shades.iter_mut().enumerate() {
        let c = &raw[i * 4..i * 4 + 4];
        *s = (c[0] as u32) << 16 | (c[1] as u32) << 8 | c[2] as u32;
    }
    Some(AccentPalette { shades })
}

/// Reads the palette from HKCU; `None` if unavailable (then use Fluent's default blue).
pub fn palette() -> Option<AccentPalette> {
    let key = windows_registry::CURRENT_USER.open(r"Software\Microsoft\Windows\CurrentVersion\Explorer\Accent").ok()?;
    let raw = key.get_value("AccentPalette").ok()?;
    parse_palette(&raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_shades() {
        let raw = [
            0xF0, 0xC0, 0xF4, 0, 0xDB, 0x9E, 0xE5, 0, 0xB7, 0x63, 0xCB, 0, 0xA9, 0x4D, 0xC1, 0, 0x8E, 0x3A, 0xA7, 0, 0x69, 0x27, 0x82, 0, 0x40, 0x0E,
            0x59, 0, 0x88, 0x17, 0x98, 0,
        ];
        let p = parse_palette(&raw).unwrap();
        assert_eq!(p.light2(), 0xDB9EE5);
        assert_eq!(p.dark1(), 0x8E3AA7);
        assert!(parse_palette(&raw[..20]).is_none());
    }
}
