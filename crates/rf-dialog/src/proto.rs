//! What the DLL asks smoothyfiles and what it gets back, over a named pipe: a little-endian u32
//! length, then that many bytes of JSON.

use std::io::{Read, Write};

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Open,
    Save,
    /// Pick a folder (FOS_PICKFOLDERS).
    Folder,
}

/// One entry of the "file type" list: "Text files" and "*.txt;*.md".
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct Filter {
    pub name: String,
    pub spec: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct Request {
    pub mode: Mode,
    /// Several files may be picked (open only).
    pub multi: bool,
    pub title: String,
    pub ok_label: String,
    /// Folder to start in (the app's choice, else its default).
    pub folder: Option<String>,
    pub file_name: String,
    pub filters: Vec<Filter>,
    /// 1-based, like the COM API.
    pub filter_index: u32,
    /// Without the dot ("txt"); added on save when the name has no extension.
    pub default_ext: String,
    pub overwrite_prompt: bool,
    /// The calling app's window (the picker stays above it).
    pub owner: isize,
    /// The calling app's exe name, for the window title.
    pub app: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, Default)]
pub struct Response {
    pub ok: bool,
    pub paths: Vec<String>,
    pub filter_index: u32,
}

pub fn write_msg<T: Serialize>(w: &mut impl Write, msg: &T) -> std::io::Result<()> {
    let body = serde_json::to_vec(msg)?;
    w.write_all(&(body.len() as u32).to_le_bytes())?;
    w.write_all(&body)?;
    w.flush()
}

pub fn read_msg<T: for<'a> Deserialize<'a>>(r: &mut impl Read) -> std::io::Result<T> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > 16 << 20 {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "message too large"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Does `name` match one of the patterns of `spec` ("*.txt;*.md", "*.*", "*")?
pub fn matches(spec: &str, name: &str) -> bool {
    let name = name.to_lowercase();
    spec.split(';').map(|p| p.trim().to_lowercase()).filter(|p| !p.is_empty()).any(|p| glob(&p, &name))
}

fn glob(p: &str, s: &str) -> bool {
    if p == "*" || p == "*.*" {
        return true;
    }
    // '*' any run, '?' one char
    let (p, s): (Vec<char>, Vec<char>) = (p.chars().collect(), s.chars().collect());
    let (mut pi, mut si, mut star, mut mark) = (0, 0, None, 0);
    while si < s.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if let Some(st) = star {
            pi = st + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// The first concrete extension of a spec ("*.txt;*.md" -> "txt"), for adding to saved names.
pub fn first_ext(spec: &str) -> Option<String> {
    spec.split(';').map(str::trim).find_map(|p| p.strip_prefix("*.").filter(|e| !e.is_empty() && !e.contains(['*', '?'])).map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let r = Request { mode: Mode::Save, title: "Kaydet".into(), filters: vec![Filter { name: "Metin".into(), spec: "*.txt".into() }], filter_index: 1, ..Default::default() };
        let mut buf = Vec::new();
        write_msg(&mut buf, &r).unwrap();
        assert_eq!(read_msg::<Request>(&mut buf.as_slice()).unwrap(), r);
    }

    #[test]
    fn patterns() {
        assert!(matches("*.txt;*.md", "Notlar.MD"));
        assert!(!matches("*.txt;*.md", "a.rs"));
        assert!(matches("*.*", "README"));
        assert!(matches("rapor-??.pdf", "rapor-01.pdf"));
        assert!(!matches("rapor-??.pdf", "rapor-1.pdf"));
        assert_eq!(first_ext("*.jpg;*.jpeg"), Some("jpg".into()));
        assert_eq!(first_ext("*.*"), None);
    }
}
