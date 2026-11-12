//! What a Linux / Android / embedded program says about itself (ELF header, program headers and
//! the dynamic section), for DirectLook's info card. Reads the first part of the file only.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ElfInfo {
    /// 32 or 64.
    pub bits: u8,
    pub little_endian: bool,
    /// "x86-64", "AArch64", "ARM", "RISC-V"…
    pub machine: String,
    pub kind: Kind,
    /// "Linux", "FreeBSD"… (when the header says; most say System V)
    pub os: Option<&'static str>,
    /// The loader it runs with ("/lib64/ld-linux-x86-64.so.2").
    pub interpreter: Option<String>,
    /// Its own library name (DT_SONAME).
    pub soname: Option<String>,
    /// The libraries it needs (DT_NEEDED).
    pub needed: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Kind {
    #[default]
    Unknown,
    /// An object file (.o).
    Relocatable,
    Executable,
    /// A shared library (.so).
    Shared,
    /// A core dump.
    Core,
}

pub fn is_elf(path: &Path) -> bool {
    let mut m = [0u8; 4];
    std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut m)).is_ok() && m == *b"\x7fELF"
}

/// Up to this much of the file is read (headers and the dynamic section are near the start).
const MAX_READ: u64 = 16 << 20;

pub fn info(path: &Path) -> Option<ElfInfo> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.seek(SeekFrom::End(0)).ok()?;
    f.seek(SeekFrom::Start(0)).ok()?;
    let mut b = Vec::new();
    f.take(len.min(MAX_READ)).read_to_end(&mut b).ok()?;
    parse(&b)
}

pub fn parse(b: &[u8]) -> Option<ElfInfo> {
    if b.get(..4)? != b"\x7fELF" {
        return None;
    }
    let bits = match b.get(4)? {
        1 => 32,
        2 => 64,
        _ => return None,
    };
    let le = match b.get(5)? {
        1 => true,
        2 => false,
        _ => return None,
    };
    let u16_at = |o: usize| -> Option<u64> {
        let s: [u8; 2] = b.get(o..o + 2)?.try_into().ok()?;
        Some(if le { u16::from_le_bytes(s) } else { u16::from_be_bytes(s) } as u64)
    };
    let u32_at = |o: usize| -> Option<u64> {
        let s: [u8; 4] = b.get(o..o + 4)?.try_into().ok()?;
        Some(if le { u32::from_le_bytes(s) } else { u32::from_be_bytes(s) } as u64)
    };
    let u64_at = |o: usize| -> Option<u64> {
        let s: [u8; 8] = b.get(o..o + 8)?.try_into().ok()?;
        Some(if le { u64::from_le_bytes(s) } else { u64::from_be_bytes(s) })
    };
    let word = |o: usize| if bits == 64 { u64_at(o) } else { u32_at(o) };
    let os = match b.get(7)? {
        3 => Some("Linux"),
        6 => Some("Solaris"),
        9 => Some("FreeBSD"),
        12 => Some("OpenBSD"),
        2 => Some("NetBSD"),
        _ => None,
    };
    let ty = u16_at(16)?;
    let machine = machine_name(u16_at(18)?);
    let (phoff, phentsize, phnum) = if bits == 64 { (u64_at(32)?, u16_at(54)?, u16_at(56)?) } else { (u32_at(28)?, u16_at(42)?, u16_at(44)?) };
    let mut out = ElfInfo { bits, little_endian: le, machine, os, ..Default::default() };
    // program headers: the loader, the loadable segments (to map addresses to file offsets) and
    // the dynamic section
    let mut loads: Vec<(u64, u64, u64)> = Vec::new(); // (vaddr, offset, filesz)
    let mut dynamic: Option<(u64, u64)> = None;
    for k in 0..phnum.min(512) {
        let h = (phoff + k * phentsize) as usize;
        // (a file cut short: what was read so far stands)
        let ph = || -> Option<(u64, u64, u64, u64)> {
            let p_type = u32_at(h)?;
            let (offset, vaddr, filesz) = if bits == 64 { (u64_at(h + 8)?, u64_at(h + 16)?, u64_at(h + 32)?) } else { (u32_at(h + 4)?, u32_at(h + 8)?, u32_at(h + 16)?) };
            Some((p_type, offset, vaddr, filesz))
        };
        let Some((p_type, offset, vaddr, filesz)) = ph() else { break };
        match p_type {
            1 => loads.push((vaddr, offset, filesz)),
            2 => dynamic = Some((offset, filesz)),
            3 => {
                if let Some(s) = b.get(offset as usize..offset.saturating_add(filesz) as usize) {
                    out.interpreter = s.split(|&c| c == 0).next().map(|s| String::from_utf8_lossy(s).into_owned());
                }
            }
            _ => {}
        }
    }
    out.kind = match ty {
        1 => Kind::Relocatable,
        2 => Kind::Executable,
        // a position-independent program is a "shared object" with a loader
        3 if out.interpreter.is_some() => Kind::Executable,
        3 => Kind::Shared,
        4 => Kind::Core,
        _ => Kind::Unknown,
    };
    if let Some((off, size)) = dynamic {
        let ent = if bits == 64 { 16 } else { 8 };
        let (mut strtab, mut needed, mut soname) = (None, Vec::new(), None);
        for k in 0..(size / ent).min(4096) {
            let e = (off + k * ent) as usize;
            let (Some(tag), Some(val)) = (word(e), word(e + ent as usize / 2)) else { break };
            match tag {
                0 => break,
                1 => needed.push(val),
                5 => strtab = Some(val),
                14 => soname = Some(val),
                _ => {}
            }
        }
        if let Some(addr) = strtab {
            // the string table's address → where it is in the file
            if let Some(base) = loads.iter().find(|(v, _, sz)| addr >= *v && addr < v + sz).map(|(v, o, _)| addr - v + o) {
                let s = |i: u64| -> Option<String> {
                    let st = b.get((base + i) as usize..)?;
                    Some(String::from_utf8_lossy(st.split(|&c| c == 0).next()?).into_owned())
                };
                out.needed = needed.into_iter().filter_map(s).filter(|n| !n.is_empty()).collect();
                out.soname = soname.and_then(s);
            }
        }
    }
    Some(out)
}

fn machine_name(m: u64) -> String {
    match m {
        3 => "x86",
        62 => "x86-64",
        40 => "ARM",
        183 => "AArch64",
        243 => "RISC-V",
        8 => "MIPS",
        20 => "PowerPC",
        21 => "PowerPC 64",
        22 => "S/390",
        2 => "SPARC",
        43 => "SPARC V9",
        50 => "IA-64",
        258 => "LoongArch",
        247 => "BPF",
        94 => "Xtensa",
        83 => "AVR",
        105 => "MSP430",
        _ => return format!("{m}"),
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hand-made 64-bit little-endian PIE: interpreter, a load segment holding the string
    /// table, and a dynamic section naming two libraries and its own name.
    #[test]
    fn reads_a_program() {
        let mut b = vec![0u8; 0x400];
        b[..4].copy_from_slice(b"\x7fELF");
        b[4] = 2;
        b[5] = 1;
        b[7] = 3;
        b[16..18].copy_from_slice(&3u16.to_le_bytes()); // ET_DYN
        b[18..20].copy_from_slice(&62u16.to_le_bytes()); // x86-64
        b[32..40].copy_from_slice(&64u64.to_le_bytes()); // phoff
        b[54..56].copy_from_slice(&56u16.to_le_bytes());
        b[56..58].copy_from_slice(&3u16.to_le_bytes());
        let ph = |b: &mut Vec<u8>, k: usize, ty: u32, off: u64, vaddr: u64, sz: u64| {
            let h = 64 + k * 56;
            b[h..h + 4].copy_from_slice(&ty.to_le_bytes());
            b[h + 8..h + 16].copy_from_slice(&off.to_le_bytes());
            b[h + 16..h + 24].copy_from_slice(&vaddr.to_le_bytes());
            b[h + 32..h + 40].copy_from_slice(&sz.to_le_bytes());
        };
        let interp = b"/lib64/ld-linux-x86-64.so.2\0";
        b[0x200..0x200 + interp.len()].copy_from_slice(interp);
        ph(&mut b, 0, 3, 0x200, 0x200, interp.len() as u64);
        // strings at file 0x300, mapped at 0x10300
        let strs = b"\0libc.so.6\0libm.so.6\0libornek.so.1\0";
        b[0x300..0x300 + strs.len()].copy_from_slice(strs);
        ph(&mut b, 1, 1, 0, 0x10000, 0x400);
        // dynamic at 0x280: NEEDED 1, NEEDED 11, SONAME 21, STRTAB 0x10300, NULL
        let dy = [(1u64, 1u64), (1, 11), (14, 21), (5, 0x10300), (0, 0)];
        for (k, (t, v)) in dy.iter().enumerate() {
            b[0x280 + k * 16..0x288 + k * 16].copy_from_slice(&t.to_le_bytes());
            b[0x288 + k * 16..0x290 + k * 16].copy_from_slice(&v.to_le_bytes());
        }
        ph(&mut b, 2, 2, 0x280, 0x10280, 80);
        let i = parse(&b).unwrap();
        assert_eq!((i.bits, i.little_endian, i.machine.as_str(), i.kind, i.os), (64, true, "x86-64", Kind::Executable, Some("Linux")));
        assert_eq!(i.interpreter.as_deref(), Some("/lib64/ld-linux-x86-64.so.2"));
        assert_eq!(i.needed, ["libc.so.6", "libm.so.6"]);
        assert_eq!(i.soname.as_deref(), Some("libornek.so.1"));
        // cut short: no panic, still the header
        assert_eq!(parse(&b[..80]).map(|i| i.machine), Some("x86-64".into()));
        assert!(parse(b"MZ\x90\0").is_none());
    }
}
