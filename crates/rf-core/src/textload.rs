//! Metin dosyası yükleme: kodlama tanıma (BOM, UTF-16, UTF-8, chardetng ile eski kod sayfaları)
//! ve büyük dosyalar için satır dizini (dosya parça parça okunur, yalnız istenen satırlar çözülür).

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use encoding_rs::{Encoding, UTF_8, UTF_16BE, UTF_16LE};
use parking_lot::{Mutex, RwLock};

/// Tanıma için okunan bayt sayısı.
const SNIFF: usize = 64 * 1024;
/// Dizinleme parçası.
const CHUNK: usize = 4 << 20;

/// Eski kod sayfası tahmininde kullanılan ülke ipucu (ör. "tr"); uygulama açılışta ayarlar.
static LOCALE_TLD: RwLock<Option<&'static [u8]>> = RwLock::new(Some(b"tr"));

pub fn set_locale_hint(tld: Option<&'static [u8]>) {
    *LOCALE_TLD.write() = tld;
}

fn utf8_valid_prefix(b: &[u8]) -> bool {
    match std::str::from_utf8(b) {
        Ok(_) => true,
        // sonda yarım kalmış bir karakter: parça sınırı, geçerli say
        Err(e) => e.error_len().is_none() && b.len() - e.valid_up_to() < 4,
    }
}

/// Kodlama ve BOM uzunluğu.
pub fn detect(head: &[u8]) -> (&'static Encoding, usize) {
    if let Some((enc, bom)) = Encoding::for_bom(head) {
        return (enc, bom);
    }
    // BOM'suz UTF-16: ASCII ağırlıklı metinde her iki bayttan biri 0
    if head.len() >= 4 {
        let pairs = head.len() / 2;
        let odd0 = head.chunks_exact(2).filter(|p| p[1] == 0 && p[0] != 0).count();
        let even0 = head.chunks_exact(2).filter(|p| p[0] == 0 && p[1] != 0).count();
        if odd0 * 10 > pairs * 4 && even0 * 10 < pairs {
            return (UTF_16LE, 0);
        }
        if even0 * 10 > pairs * 4 && odd0 * 10 < pairs {
            return (UTF_16BE, 0);
        }
    }
    if utf8_valid_prefix(head) {
        return (UTF_8, 0);
    }
    let mut d = chardetng::EncodingDetector::new();
    d.feed(head, true);
    (d.guess(*LOCALE_TLD.read(), true), 0)
}

/// Baytları tanınan kodlamayla çözer.
pub fn detect_and_decode(bytes: &[u8]) -> (String, &'static str) {
    let (enc, bom) = detect(&bytes[..bytes.len().min(SNIFF)]);
    let (s, _) = enc.decode_without_bom_handling(&bytes[bom..]);
    (s.into_owned(), enc.name())
}

/// Satır satır okunan metin dosyası. Satır başları arka planda dizinlenir; dizinlenmiş her
/// satır hemen okunabilir.
pub struct TextFile {
    path: PathBuf,
    /// Read at positions (no shared cursor): the indexer's big reads never hold up a line.
    file: File,
    /// Lines read lately (a list asks for the same rows every frame while it scrolls).
    cache: Mutex<std::collections::HashMap<usize, String>>,
    enc: &'static Encoding,
    start: u64,
    len: u64,
    /// Satır başlarının bayt konumları (ilk öğe veri başlangıcı).
    starts: RwLock<Vec<u64>>,
    scanned: AtomicU64,
    done: AtomicBool,
}

impl TextFile {
    pub fn open(path: &Path) -> std::io::Result<TextFile> {
        let mut file = File::open(path)?;
        let len = file.metadata()?.len();
        let mut head = vec![0u8; SNIFF.min(len as usize)];
        file.read_exact(&mut head)?;
        let (enc, bom) = detect(&head);
        Ok(TextFile {
            path: path.to_path_buf(),
            file,
            cache: Mutex::new(std::collections::HashMap::new()),
            enc,
            start: bom as u64,
            len,
            starts: RwLock::new(vec![bom as u64]),
            scanned: AtomicU64::new(bom as u64),
            done: AtomicBool::new(len <= bom as u64),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn encoding_name(&self) -> &'static str {
        self.enc.name()
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len <= self.start
    }

    pub fn is_indexed(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    fn wide(&self) -> Option<bool> {
        if self.enc == UTF_16LE {
            Some(true)
        } else if self.enc == UTF_16BE {
            Some(false)
        } else {
            None
        }
    }

    /// Bir parça daha dizinler; dosya bittiyse false.
    pub fn index_chunk(&self) -> bool {
        if self.is_indexed() {
            return false;
        }
        let from = self.scanned.load(Ordering::Acquire);
        let mut buf = vec![0u8; CHUNK.min((self.len - from) as usize)];
        if read_at(&self.file, &mut buf, from).is_err() {
            self.done.store(true, Ordering::Release);
            return false;
        }
        let mut found = Vec::new();
        match self.wide() {
            None => found.extend(memchr::memchr_iter(b'\n', &buf).map(|i| from + i as u64 + 1)),
            Some(le) => {
                for i in memchr::memchr_iter(b'\n', &buf) {
                    let pos = from + i as u64;
                    let (lo, other) = if le { (pos, i + 1) } else { (pos - 1, i.wrapping_sub(1)) };
                    if (lo - self.start) % 2 == 0 && buf.get(other) == Some(&0) {
                        found.push(lo + 2);
                    }
                }
            }
        }
        // UTF-16'da parça sınırı çift baytta kalsın
        let next = from + buf.len() as u64;
        self.starts.write().extend(found);
        self.scanned.store(next, Ordering::Release);
        if next >= self.len {
            self.done.store(true, Ordering::Release);
            return false;
        }
        true
    }

    pub fn index_all(&self) {
        while self.index_chunk() {}
    }

    /// Dizinlenmiş satır sayısı (dosyanın sonundaki satır sonundan sonra boş satır sayılmaz).
    pub fn line_count(&self) -> usize {
        let s = self.starts.read();
        let n = s.len();
        if self.is_indexed() && n > 1 && *s.last().unwrap() >= self.len { n - 1 } else { n }
    }

    /// `i`. satır (satır sonu olmadan); dizinlenmemişse None.
    pub fn line(&self, i: usize) -> Option<String> {
        if let Some(s) = self.cache.lock().get(&i) {
            return Some(s.clone());
        }
        let (a, b) = {
            let s = self.starts.read();
            let a = *s.get(i)?;
            let b = match s.get(i + 1) {
                Some(b) => *b,
                None if self.is_indexed() => self.len,
                None => return None,
            };
            (a, b)
        };
        let mut buf = vec![0u8; (b - a).min(1 << 20) as usize];
        read_at(&self.file, &mut buf, a).ok()?;
        let (s, _) = self.enc.decode_without_bom_handling(&buf);
        let line = s.trim_end_matches(['\n', '\r']).to_string();
        let mut c = self.cache.lock();
        if c.len() >= 1024 {
            c.clear();
        }
        c.insert(i, line.clone());
        Some(line)
    }
}

/// Fills `buf` from `file` at `pos`, without moving a shared cursor.
fn read_at(file: &File, mut buf: &mut [u8], mut pos: u64) -> std::io::Result<()> {
    while !buf.is_empty() {
        #[cfg(windows)]
        let n = std::os::windows::fs::FileExt::seek_read(file, buf, pos)?;
        #[cfg(unix)]
        let n = std::os::unix::fs::FileExt::read_at(file, buf, pos)?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buf = &mut buf[n..];
        pos += n as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf16le_bom(s: &str) -> Vec<u8> {
        let mut v = vec![0xFF, 0xFE];
        for u in s.encode_utf16() {
            v.extend_from_slice(&u.to_le_bytes());
        }
        v
    }

    #[test]
    fn utf8_bom_and_utf16() {
        assert_eq!(detect_and_decode(b"\xEF\xBB\xBFmerhaba").0, "merhaba");
        assert_eq!(detect_and_decode(&utf16le_bom("şöğ")).0, "şöğ");
    }

    #[test]
    fn windows_1254_turkish() {
        let (s, enc) = detect_and_decode(&[0xFE, 0xFD, 0xF0, 0x20, 0x61]);
        assert_eq!(s, "şığ a");
        assert_eq!(enc, "windows-1254");
    }

    #[test]
    fn plain_utf8() {
        assert_eq!(detect_and_decode("çay ve şeker".as_bytes()), ("çay ve şeker".to_string(), "UTF-8"));
    }

    #[test]
    fn line_index_reads_lines_by_number() {
        let path = std::env::temp_dir().join(format!("rf-textload-{}.txt", std::process::id()));
        let mut body = String::new();
        for i in 0..10_000 {
            body.push_str(&format!("satır {i}\r\n"));
        }
        std::fs::write(&path, body.as_bytes()).unwrap();
        let f = TextFile::open(&path).unwrap();
        f.index_all();
        assert_eq!(f.line_count(), 10_000);
        assert_eq!(f.line(0).unwrap(), "satır 0");
        assert_eq!(f.line(9_999).unwrap(), "satır 9999");
        assert_eq!(f.encoding_name(), "UTF-8");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn line_index_utf16() {
        let path = std::env::temp_dir().join(format!("rf-textload16-{}.txt", std::process::id()));
        std::fs::write(&path, utf16le_bom("bir\niki\nüç")).unwrap();
        let f = TextFile::open(&path).unwrap();
        f.index_all();
        assert_eq!(f.line_count(), 3);
        assert_eq!(f.line(2).unwrap(), "üç");
        let _ = std::fs::remove_file(&path);
    }
}
