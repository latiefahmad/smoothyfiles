//! What an app package says about itself, for DirectLook's info card: Windows packages (MSIX,
//! APPX and their bundles), Android packages (APK, its binary AndroidManifest.xml), iOS apps (IPA,
//! the app's Info.plist, binary or XML) and NuGet packages. Read from the zip's directory and one
//! or two small members; nothing is extracted to disk.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

mod vector;

/// The kind of package (the card names it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Msix,
    MsixBundle,
    Apk,
    Ipa,
    Nuget,
}

#[derive(Debug, Clone, Default)]
pub struct AppInfo {
    /// The name people see ("Paint"), when the package gives one in plain text.
    pub name: Option<String>,
    /// The package's own id ("Microsoft.Paint", "com.example.app").
    pub id: Option<String>,
    pub version: Option<String>,
    pub publisher: Option<String>,
    /// Processor architectures ("x64", "arm64"…).
    pub arch: Vec<String>,
    /// The oldest system it runs on ("Windows 10.0.17763.0", "Android 8.0 (API 26)", "iOS 15.0").
    pub min_os: Option<String>,
    /// Permissions asked for (Android) or capabilities declared (Windows).
    pub permissions: usize,
    pub description: Option<String>,
    /// The app's icon as PNG or WebP bytes, when the package carries a usable one ([`icon`]
    /// decodes it).
    pub icon_png: Option<Vec<u8>>,
}

/// The package icon, decoded and at most `max` pixels on a side.
pub fn icon(bytes: &[u8], max: u32) -> Option<crate::thumbs::Bgra> {
    let img = image::load_from_memory(bytes).ok()?;
    let img = if img.width() > max || img.height() > max { img.resize(max, max, image::imageops::FilterType::Triangle) } else { img };
    let mut rgba = img.into_rgba8();
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    Some(crate::thumbs::Bgra { width: rgba.width(), height: rgba.height(), data: rgba.into_raw() })
}

fn is_picture(v: &[u8]) -> bool {
    v.starts_with(b"\x89PNG") || (v.len() > 12 && &v[..4] == b"RIFF" && &v[8..12] == b"WEBP")
}

pub fn kind_of(path: &Path) -> Option<Kind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "msix" | "appx" => Kind::Msix,
        "msixbundle" | "appxbundle" => Kind::MsixBundle,
        "apk" => Kind::Apk,
        "ipa" => Kind::Ipa,
        "nupkg" | "snupkg" => Kind::Nuget,
        _ => return None,
    })
}

/// Members larger than this are not read (a manifest is a few KB; an icon well under this).
const MAX_MEMBER: u64 = 8 << 20;

pub fn info(path: &Path) -> Option<(Kind, AppInfo)> {
    let kind = kind_of(path)?;
    let mut zip = zip::ZipArchive::new(BufReader::new(File::open(path).ok()?)).ok()?;
    let info = match kind {
        Kind::Msix => msix(&mut zip),
        Kind::MsixBundle => bundle(&mut zip),
        Kind::Apk => apk(&mut zip),
        Kind::Ipa => ipa(&mut zip),
        Kind::Nuget => nuget(&mut zip),
    }?;
    Some((kind, info))
}

type Zip = zip::ZipArchive<BufReader<File>>;

fn member(zip: &mut Zip, name: &str) -> Option<Vec<u8>> {
    member_up_to(zip, name, MAX_MEMBER)
}

fn member_up_to(zip: &mut Zip, name: &str, max: u64) -> Option<Vec<u8>> {
    let mut f = zip.by_name(name).ok()?;
    if f.size() > max {
        return None;
    }
    let mut v = Vec::with_capacity(f.size() as usize);
    f.read_to_end(&mut v).ok()?;
    Some(v)
}

fn names(zip: &Zip) -> Vec<String> {
    zip.file_names().filter_map(|n| n.ok().map(|n| n.into_owned())).collect()
}

// ------------------------------------------------------------------ XML, the little needed

/// The attributes of the first `<tag …>` in `xml` (namespace prefixes dropped from names).
fn tag_attrs(xml: &str, tag: &str) -> Option<Vec<(String, String)>> {
    let mut from = 0;
    loop {
        let i = from + xml[from..].find(&format!("<{tag}"))?;
        let rest = &xml[i + 1 + tag.len()..];
        // "<Identity" must not match "<IdentityX"
        if rest.starts_with(|c: char| c.is_whitespace() || c == '/' || c == '>') {
            let end = rest.find('>')?;
            return Some(attrs(&rest[..end]));
        }
        from = i + 1;
    }
}

fn attrs(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b'/') {
            i += 1;
        }
        let k0 = i;
        while i < b.len() && b[i] != b'=' && !b[i].is_ascii_whitespace() {
            i += 1;
        }
        let key = &s[k0..i];
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() || b[i] != b'=' {
            i += 1;
            continue;
        }
        i += 1;
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let Some(&q) = b.get(i) else { break };
        if q != b'"' && q != b'\'' {
            continue;
        }
        let v0 = i + 1;
        let Some(len) = s[v0..].find(q as char) else { break };
        let key = key.rsplit(':').next().unwrap_or(key).to_string();
        out.push((key, unescape(&s[v0..v0 + len])));
        i = v0 + len + 1;
    }
    out
}

fn attr<'a>(a: &'a [(String, String)], key: &str) -> Option<&'a str> {
    a.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str()).filter(|v| !v.is_empty())
}

/// The text of the first `<tag>…</tag>`.
fn tag_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let mut from = 0;
    loop {
        let i = from + xml[from..].find(&open)?;
        let rest = &xml[i + open.len()..];
        if rest.starts_with(|c: char| c.is_whitespace() || c == '>') {
            let start = rest.find('>')? + 1;
            let end = rest.find(&format!("</{tag}>"))?;
            let t = unescape(rest.get(start..end)?.trim());
            return (!t.is_empty()).then_some(t);
        }
        from = i + 1;
    }
}

fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';').filter(|&e| e < 12) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..end];
        let c = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            e if e.starts_with("#x") => u32::from_str_radix(&e[2..], 16).ok().and_then(char::from_u32),
            e if e.starts_with('#') => e[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match c {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// "ms-resource:AppName" and the like are references into the package's resources, not names.
fn plain(s: Option<String>) -> Option<String> {
    s.filter(|v| !v.starts_with("ms-resource:") && !v.starts_with('@'))
}

// ------------------------------------------------------------------ Windows

fn msix(zip: &mut Zip) -> Option<AppInfo> {
    let xml = String::from_utf8(member(zip, "AppxManifest.xml")?).ok()?;
    let id = tag_attrs(&xml, "Identity").unwrap_or_default();
    let mut i = AppInfo {
        id: attr(&id, "Name").map(str::to_string),
        version: attr(&id, "Version").map(str::to_string),
        arch: attr(&id, "ProcessorArchitecture").map(|a| vec![a.to_string()]).unwrap_or_default(),
        name: plain(tag_text(&xml, "DisplayName")),
        publisher: plain(tag_text(&xml, "PublisherDisplayName")).or_else(|| attr(&id, "Publisher").map(publisher_cn)),
        description: plain(tag_text(&xml, "Description")),
        ..Default::default()
    };
    if let Some(dev) = tag_attrs(&xml, "TargetDeviceFamily") {
        i.min_os = attr(&dev, "MinVersion").map(|v| format!("Windows {v}"));
    }
    i.permissions = xml.matches("<Capability ").count() + xml.matches("<rescap:Capability ").count() + xml.matches("<DeviceCapability ").count();
    // the logo: "Assets\StoreLogo.png" is stored with a scale in its name (StoreLogo.scale-200.png)
    if let Some(logo) = tag_text(&xml, "Logo") {
        let logo = logo.replace('\\', "/");
        let (stem, ext) = logo.rsplit_once('.').unwrap_or((&logo, "png"));
        let all = names(zip);
        let best = all
            .iter()
            .filter(|n| n.eq_ignore_ascii_case(&logo) || (n.to_lowercase().starts_with(&format!("{}.", stem.to_lowercase())) && n.to_lowercase().ends_with(&format!(".{}", ext.to_lowercase()))))
            .max_by_key(|n| scale_of(n))
            .cloned();
        if let Some(b) = best {
            i.icon_png = member(zip, &b).filter(|v| is_picture(v));
        }
    }
    Some(i)
}

/// "scale-200" → 200 (a larger, sharper logo first; the plain name counts as 100).
fn scale_of(name: &str) -> u32 {
    let l = name.to_lowercase();
    l.find("scale-").and_then(|i| l[i + 6..].split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()).or_else(|| l.find("targetsize-").and_then(|i| l[i + 11..].split(|c: char| !c.is_ascii_digit()).next()?.parse().ok())).unwrap_or(100)
}

/// "CN=Microsoft Corporation, O=…" → "Microsoft Corporation".
fn publisher_cn(p: &str) -> String {
    p.split(',').find_map(|part| part.trim().strip_prefix("CN=")).unwrap_or(p).trim_matches('"').to_string()
}

fn bundle(zip: &mut Zip) -> Option<AppInfo> {
    let xml = String::from_utf8(member(zip, "AppxMetadata/AppxBundleManifest.xml")?).ok()?;
    let id = tag_attrs(&xml, "Identity").unwrap_or_default();
    let mut i = AppInfo {
        id: attr(&id, "Name").map(str::to_string),
        version: attr(&id, "Version").map(str::to_string),
        publisher: attr(&id, "Publisher").map(publisher_cn),
        ..Default::default()
    };
    // one <Package Type="application" Architecture="x64" …/> per architecture
    let mut from = 0;
    while let Some(k) = xml[from..].find("<Package ") {
        let start = from + k;
        let end = start + xml[start..].find('>').unwrap_or(0);
        let a = attrs(&xml[start + 9..end]);
        if attr(&a, "Type") == Some("application") {
            if let Some(arch) = attr(&a, "Architecture") {
                if !i.arch.iter().any(|x| x == arch) {
                    i.arch.push(arch.to_string());
                }
            }
        }
        from = end.max(start + 1);
    }
    Some(i)
}

// ------------------------------------------------------------------ NuGet

fn nuget(zip: &mut Zip) -> Option<AppInfo> {
    let spec = names(zip).into_iter().find(|n| !n.contains('/') && n.to_lowercase().ends_with(".nuspec"))?;
    let xml = String::from_utf8(member(zip, &spec)?).ok()?;
    Some(AppInfo {
        id: tag_text(&xml, "id"),
        version: tag_text(&xml, "version"),
        name: tag_text(&xml, "title"),
        publisher: tag_text(&xml, "authors"),
        description: tag_text(&xml, "description").map(|d| d.split_whitespace().collect::<Vec<_>>().join(" ")),
        ..Default::default()
    })
}

// ------------------------------------------------------------------ Android

fn apk(zip: &mut Zip) -> Option<AppInfo> {
    let m = member(zip, "AndroidManifest.xml")?;
    let mut i = AppInfo::default();
    let mut min_sdk = None;
    let (mut label_ref, mut icon_ref) = (None, None);
    axml(&m, &mut |tag, a| match tag {
        "manifest" => {
            i.id = a.get("package").cloned();
            i.version = match (a.get("versionName"), a.get("versionCode")) {
                (Some(n), Some(c)) => Some(format!("{n} ({c})")),
                (Some(n), None) => Some(n.clone()),
                (None, c) => c.cloned(),
            };
        }
        "uses-sdk" => min_sdk = a.get("minSdkVersion").cloned(),
        "uses-permission" | "uses-permission-sdk-23" => i.permissions += 1,
        "application" => {
            let label = a.get("label").cloned();
            label_ref = label.as_deref().and_then(res_id);
            i.name = plain(label);
            icon_ref = a.get("icon").or(a.get("roundIcon")).and_then(|s| res_id(s));
        }
        _ => {}
    })?;
    // the name and the icon are usually references into resources.arsc
    let arsc = (label_ref.is_some() || icon_ref.is_some()).then(|| member_up_to(zip, "resources.arsc", 64 << 20)).flatten();
    if let (Some(arsc), Some(id)) = (&arsc, label_ref) {
        if i.name.is_none() {
            i.name = arsc_values(arsc, id).into_iter().filter_map(|v| Some((v.language, v.string?))).min_by_key(|(lang, _)| *lang != [0, 0]).map(|(_, s)| s);
        }
    }
    if let (Some(arsc), Some(id)) = (&arsc, icon_ref) {
        i.icon_png = apk_icon(zip, arsc, id, 0);
    }
    i.min_os = min_sdk.map(|s| match s.parse::<u32>().ok().and_then(android_version) {
        Some(v) => format!("Android {v} (API {s})"),
        None => format!("API {s}"),
    });
    // the native code says which processors it runs on (none: any)
    let all = names(zip);
    for n in &all {
        if let Some(abi) = n.strip_prefix("lib/").and_then(|r| r.split('/').next()) {
            if !abi.is_empty() && !i.arch.iter().any(|x| x == abi) {
                i.arch.push(abi.to_string());
            }
        }
    }
    // the launcher icon, by the usual names (its real name hides in resources.arsc): the
    // densest picture
    let density = |n: &str| ["xxxhdpi", "xxhdpi", "xhdpi", "hdpi", "mdpi"].iter().position(|d| n.contains(d)).map_or(9, |p| p);
    let icon = all
        .iter()
        .filter(|n| n.starts_with("res/") && n.ends_with(".png") && n.contains("ic_launcher") && !n.contains("foreground") && !n.contains("background") && !n.contains("monochrome"))
        .min_by_key(|n| (density(n), n.contains("round")))
        .cloned();
    if i.icon_png.is_none() {
        if let Some(n) = icon {
            i.icon_png = member(zip, &n).filter(|v| is_picture(v));
        }
    }
    Some(i)
}

/// "@0x7f0d0001" → 0x7f0d0001.
fn res_id(s: &str) -> Option<u32> {
    u32::from_str_radix(s.strip_prefix("@0x")?, 16).ok()
}

/// The icon resource `id`: the densest bitmap among its versions; an adaptive icon (XML) gives
/// its foreground's.
fn apk_icon(zip: &mut Zip, arsc: &[u8], id: u32, depth: u8) -> Option<Vec<u8>> {
    let mut files: Vec<(u16, String)> = arsc_values(arsc, id).into_iter().filter_map(|v| Some((v.density, v.string?))).filter(|(_, f)| f.starts_with("res/")).collect();
    // densest first ("any density" 0xfffe / 0xffff is a vector or adaptive XML: last)
    files.sort_by_key(|(d, f)| (f.ends_with(".xml"), std::cmp::Reverse(if *d >= 0xfffe { 0 } else { *d })));
    for (_, f) in &files {
        if f.ends_with(".xml") {
            // a vector or an adaptive icon (its layers): drawn here
            let Some(x) = member(zip, f) else { continue };
            if let Some(v) = vector::render(zip, arsc, &x, 192) {
                return Some(v);
            }
            if depth > 0 {
                continue;
            }
            // (could not draw it: the foreground's bitmap, if it has one)
            let mut fg = None;
            axml(&x, &mut |tag, a| {
                if tag == "foreground" && fg.is_none() {
                    fg = a.get("drawable").and_then(|s| res_id(s));
                }
            });
            if let Some(fg) = fg {
                if let Some(v) = apk_icon(zip, arsc, fg, depth + 1) {
                    return Some(v);
                }
            }
        } else if let Some(v) = member(zip, f).filter(|v| is_picture(v)) {
            return Some(v);
        }
    }
    None
}

/// One version of a resource value (per configuration).
struct ResValue {
    density: u16,
    language: [u8; 2],
    string: Option<String>,
    /// The raw value: its type (0x01 reference, 0x03 string, 0x1c–0x1f colour…) and data.
    dtype: u8,
    data: u32,
}

/// The values of resource `id` in a compiled resource table, one per configuration.
fn arsc_values(b: &[u8], id: u32) -> Vec<ResValue> {
    arsc_values_inner(b, id).unwrap_or_default()
}

fn arsc_values_inner(b: &[u8], id: u32) -> Option<Vec<ResValue>> {
    let u16_at = |o: usize| Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?));
    let u32_at = |o: usize| Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?));
    if u16_at(0)? != 0x0002 {
        return None;
    }
    let (want_pkg, want_type, want_entry) = ((id >> 24) as u32, ((id >> 16) & 0xff) as u8, (id & 0xffff) as usize);
    let mut out = Vec::new();
    let mut strings: Vec<String> = Vec::new();
    let mut o = u16_at(2)? as usize;
    while o + 8 <= b.len() {
        let (ty, size) = (u16_at(o)?, u32_at(o + 4)? as usize);
        if size < 8 || o + size > b.len() {
            break;
        }
        match ty {
            0x0001 => strings = string_pool(&b[o..o + size])?,
            0x0200 if u32_at(o + 8)? == want_pkg => {
                // the package's chunks: string pools, type specs, types
                let mut p = o + u16_at(o + 2)? as usize;
                while p + 8 <= o + size {
                    let (pty, psize) = (u16_at(p)?, u32_at(p + 4)? as usize);
                    if psize < 8 || p + psize > o + size {
                        break;
                    }
                    if pty == 0x0201 && *b.get(p + 8)? == want_type {
                        let flags = *b.get(p + 9)?;
                        let count = u32_at(p + 12)? as usize;
                        let entries = p + u32_at(p + 16)? as usize;
                        let config = p + 20;
                        let density = u16_at(config + 14).unwrap_or(0);
                        let language = [*b.get(config + 8)?, *b.get(config + 9)?];
                        let header = p + u16_at(p + 2)? as usize;
                        // where the entry is: sparse tables list (index, offset / 4) pairs
                        let off = if flags & 0x01 != 0 {
                            (0..count).find_map(|k| (u16_at(header + k * 4)? as usize == want_entry).then(|| u16_at(header + k * 4 + 2).map(|v| v as usize * 4)).flatten())
                        } else if want_entry < count {
                            if flags & 0x02 != 0 {
                                u16_at(header + want_entry * 2).filter(|&v| v != 0xffff).map(|v| v as usize * 4)
                            } else {
                                u32_at(header + want_entry * 4).filter(|&v| v != u32::MAX).map(|v| v as usize)
                            }
                        } else {
                            None
                        };
                        if let Some(off) = off {
                            let e = entries + off;
                            let (esize, eflags) = (u16_at(e)? as usize, u16_at(e + 2)?);
                            let (dtype, data) = if eflags & 0x0008 != 0 {
                                ((eflags >> 8) as u8, u32_at(e + 4)?)
                            } else if eflags & 0x0001 == 0 {
                                (*b.get(e + esize + 3)?, u32_at(e + esize + 4)?)
                            } else {
                                (0, 0) // a map (style, plural…): not a single value
                            };
                            let string = (dtype == 0x03).then(|| strings.get(data as usize).cloned()).flatten();
                            out.push(ResValue { density, language, string, dtype, data });
                        }
                    }
                    p += psize;
                }
            }
            _ => {}
        }
        o += size;
    }
    Some(out)
}

fn android_version(api: u32) -> Option<&'static str> {
    Some(match api {
        21 => "5.0",
        22 => "5.1",
        23 => "6.0",
        24 => "7.0",
        25 => "7.1",
        26 => "8.0",
        27 => "8.1",
        28 => "9",
        29 => "10",
        30 => "11",
        31 => "12",
        32 => "12L",
        33 => "13",
        34 => "14",
        35 => "15",
        36 => "16",
        _ => return None,
    })
}

/// Walks Android's binary XML: calls `on_tag(name, attributes)` for each start tag. Attribute
/// values are strings as written, numbers in decimal, references as "@0x7f…", colours as
/// "#aarrggbb", dimensions as "12dp", fractions as "16%".
fn axml(b: &[u8], on_tag: &mut dyn FnMut(&str, &std::collections::HashMap<String, String>)) -> Option<()> {
    axml_events(b, &mut |ev| {
        if let Some((tag, a)) = ev {
            on_tag(tag, a);
        }
    })
}

/// A compiled dimension or fraction's number.
fn complex(data: u32) -> f32 {
    const MULT: [f32; 4] = [1.0 / 256.0, 1.0 / 32768.0, 1.0 / 8388608.0, 1.0 / 2147483648.0];
    ((data & 0xffff_ff00) as i32) as f32 * MULT[((data >> 4) & 3) as usize]
}

/// [`axml`] with the end tags too (`None`), for nested drawables.
fn axml_events(b: &[u8], on: &mut dyn FnMut(Option<(&str, &std::collections::HashMap<String, String>)>)) -> Option<()> {
    let u16_at = |o: usize| Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?));
    let u32_at = |o: usize| Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?));
    if u16_at(0)? != 0x0003 {
        return None;
    }
    let mut strings: Vec<String> = Vec::new();
    let mut o = u16_at(2)? as usize;
    while o + 8 <= b.len() {
        let ty = u16_at(o)?;
        let header = u16_at(o + 2)? as usize;
        let size = u32_at(o + 4)? as usize;
        if size < 8 || o + size > b.len() {
            break;
        }
        match ty {
            0x0001 => strings = string_pool(&b[o..o + size])?,
            0x0102 => {
                let name = u32_at(o + 20)? as usize;
                let attr_start = u16_at(o + 24)? as usize;
                let attr_size = u16_at(o + 26)? as usize;
                let count = u16_at(o + 28)? as usize;
                let mut attrs = std::collections::HashMap::new();
                for k in 0..count {
                    let a = o + 16 + attr_start + k * attr_size.max(20);
                    let (an, raw, dtype, data) = (u32_at(a + 4)? as usize, u32_at(a + 8)?, *b.get(a + 15)?, u32_at(a + 16)?);
                    let Some(key) = strings.get(an) else { continue };
                    let value = match dtype {
                        0x03 => strings.get(data as usize).cloned().unwrap_or_default(),
                        0x01 => format!("@0x{data:08x}"),
                        0x10 => (data as i32).to_string(),
                        0x11 => format!("0x{data:x}"),
                        0x12 => (data != 0).to_string(),
                        0x04 => f32::from_bits(data).to_string(),
                        0x05 => {
                            let unit = ["px", "dp", "sp", "pt", "in", "mm"].get((data & 0xf) as usize).copied().unwrap_or("");
                            format!("{}{unit}", complex(data))
                        }
                        0x06 => format!("{}%{}", complex(data) * 100.0, if data & 0xf == 1 { "p" } else { "" }),
                        0x1c | 0x1e => format!("#{data:08x}"),
                        0x1d | 0x1f => format!("#{:08x}", data | 0xff00_0000),
                        0x02 => format!("?0x{data:08x}"),
                        _ if raw != u32::MAX => strings.get(raw as usize).cloned().unwrap_or_default(),
                        _ => data.to_string(),
                    };
                    attrs.insert(key.clone(), value);
                }
                if let Some(tag) = strings.get(name) {
                    on(Some((tag, &attrs)));
                }
            }
            0x0103 => on(None),
            _ => {}
        }
        let _ = header;
        o += size;
    }
    Some(())
}

fn string_pool(c: &[u8]) -> Option<Vec<String>> {
    let u16_at = |o: usize| Some(u16::from_le_bytes(c.get(o..o + 2)?.try_into().ok()?));
    let u32_at = |o: usize| Some(u32::from_le_bytes(c.get(o..o + 4)?.try_into().ok()?));
    let count = u32_at(8)? as usize;
    let flags = u32_at(16)?;
    let strings_start = u32_at(20)? as usize;
    let utf8 = flags & 0x100 != 0;
    let header = u16_at(2)? as usize;
    let mut out = Vec::with_capacity(count.min(1 << 16));
    for k in 0..count.min(1 << 16) {
        let off = u32_at(header + k * 4)? as usize;
        let mut p = strings_start + off;
        let s = if utf8 {
            // lengths in UTF-16 units then in bytes, each 1 or 2 bytes
            let skip = |p: &mut usize| -> Option<usize> {
                let a = *c.get(*p)? as usize;
                *p += 1;
                if a & 0x80 != 0 {
                    let b = *c.get(*p)? as usize;
                    *p += 1;
                    Some(((a & 0x7f) << 8) | b)
                } else {
                    Some(a)
                }
            };
            skip(&mut p)?;
            let n = skip(&mut p)?;
            String::from_utf8_lossy(c.get(p..p + n)?).into_owned()
        } else {
            let mut n = u16_at(p)? as usize;
            p += 2;
            if n & 0x8000 != 0 {
                n = ((n & 0x7fff) << 16) | u16_at(p)? as usize;
                p += 2;
            }
            let units: Vec<u16> = (0..n).map(|k| u16_at(p + k * 2)).collect::<Option<_>>()?;
            String::from_utf16_lossy(&units)
        };
        out.push(s);
    }
    Some(out)
}

// ------------------------------------------------------------------ iOS

fn ipa(zip: &mut Zip) -> Option<AppInfo> {
    // Payload/Name.app/Info.plist (not the plists of frameworks and extensions deeper down)
    let plist_name = names(zip).into_iter().find(|n| {
        let parts: Vec<&str> = n.split('/').collect();
        parts.len() == 3 && parts[0] == "Payload" && parts[1].ends_with(".app") && parts[2] == "Info.plist"
    })?;
    let data = member(zip, &plist_name)?;
    let d = plist_strings(&data)?;
    let get = |k: &str| d.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone()).filter(|v| !v.is_empty());
    let version = match (get("CFBundleShortVersionString"), get("CFBundleVersion")) {
        (Some(a), Some(b)) if a != b => Some(format!("{a} ({b})")),
        (a, b) => a.or(b),
    };
    Some(AppInfo {
        name: get("CFBundleDisplayName").or_else(|| get("CFBundleName")),
        id: get("CFBundleIdentifier"),
        version,
        min_os: get("MinimumOSVersion").map(|v| format!("iOS {v}")),
        ..Default::default()
    })
}

/// The top-level string (and integer) values of a property list, binary or XML.
fn plist_strings(data: &[u8]) -> Option<Vec<(String, String)>> {
    if data.starts_with(b"bplist00") {
        return bplist(data);
    }
    let xml = std::str::from_utf8(data).ok()?;
    let mut out = Vec::new();
    let mut rest = xml.split_once("<dict>")?.1;
    while let Some(k) = rest.find("<key>") {
        rest = &rest[k + 5..];
        let end = rest.find("</key>")?;
        let key = unescape(&rest[..end]);
        rest = rest[end + 6..].trim_start();
        let value = if let Some(r) = rest.strip_prefix("<string>") {
            r.find("</string>").map(|e| unescape(&r[..e]))
        } else if let Some(r) = rest.strip_prefix("<integer>") {
            r.find("</integer>").map(|e| r[..e].to_string())
        } else {
            None
        };
        if let Some(v) = value {
            out.push((key, v));
        }
        // a nested dict or array: its keys are not top level, skip past it
        for (open, close) in [("<dict>", "</dict>"), ("<array>", "</array>")] {
            if rest.starts_with(open) {
                if let Some(e) = rest.find(close) {
                    rest = &rest[e + close.len()..];
                }
            }
        }
    }
    Some(out)
}

fn bplist(b: &[u8]) -> Option<Vec<(String, String)>> {
    let t = b.len().checked_sub(32)?;
    let off_size = *b.get(t + 6)? as usize;
    let ref_size = *b.get(t + 7)? as usize;
    let be = |s: &[u8]| s.iter().fold(0u64, |a, &x| (a << 8) | x as u64);
    let count = be(b.get(t + 8..t + 16)?) as usize;
    let top = be(b.get(t + 16..t + 24)?) as usize;
    let table = be(b.get(t + 24..t + 32)?) as usize;
    if off_size == 0 || ref_size == 0 || count > 1 << 20 {
        return None;
    }
    let offset = |obj: usize| -> Option<usize> { Some(be(b.get(table + obj * off_size..table + (obj + 1) * off_size)?) as usize) };
    // an object's (type, length, where its data starts)
    let head = |o: usize| -> Option<(u8, usize, usize)> {
        let m = *b.get(o)?;
        let (ty, low) = (m >> 4, (m & 0x0f) as usize);
        if low == 0x0f && matches!(ty, 0x4..=0x6 | 0xa | 0xd) {
            let im = *b.get(o + 1)?;
            let n = 1usize << (im & 0x0f);
            Some((ty, be(b.get(o + 2..o + 2 + n)?) as usize, o + 2 + n))
        } else {
            Some((ty, low, o + 1))
        }
    };
    let string = |obj: usize| -> Option<String> {
        let (ty, len, at) = head(offset(obj)?)?;
        match ty {
            0x5 => Some(String::from_utf8_lossy(b.get(at..at + len)?).into_owned()),
            0x6 => {
                let units: Vec<u16> = (0..len).map(|k| Some(u16::from_be_bytes(b.get(at + k * 2..at + k * 2 + 2)?.try_into().ok()?))).collect::<Option<_>>()?;
                Some(String::from_utf16_lossy(&units))
            }
            0x1 => {
                let n = 1usize << len;
                Some((be(b.get(at..at + n)?) as i64).to_string())
            }
            _ => None,
        }
    };
    let (ty, n, at) = head(offset(top)?)?;
    if ty != 0xd {
        return None;
    }
    let r = |k: usize| -> Option<usize> { Some(be(b.get(at + k * ref_size..at + (k + 1) * ref_size)?) as usize) };
    let mut out = Vec::new();
    for k in 0..n.min(4096) {
        let (Some(kr), Some(vr)) = (r(k), r(n + k)) else { break };
        if let (Some(key), Some(v)) = (string(kr), string(vr)) {
            out.push((key, v));
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A binary XML document of nested elements (each inside the one before), attributes given
    /// as (name, type, data); a string attribute (type 3) takes `text`.
    pub(super) fn axml_doc(elems: &[(&str, &[(&str, u8, u32)])], text: &str) -> Vec<u8> {
        let mut strings: Vec<String> = Vec::new();
        let mut sid = |s: &str| -> u32 {
            if let Some(i) = strings.iter().position(|x| x == s) {
                return i as u32;
            }
            strings.push(s.to_string());
            strings.len() as u32 - 1
        };
        let text_id = sid(text);
        let mut chunks = Vec::new();
        for (tag, attrs) in elems {
            let name = sid(tag);
            let mut c = Vec::new();
            c.extend(0x0102u16.to_le_bytes());
            c.extend(16u16.to_le_bytes());
            c.extend(((36 + 20 * attrs.len()) as u32).to_le_bytes());
            c.extend([0u8; 8]); // line, comment
            c.extend(u32::MAX.to_le_bytes()); // namespace
            c.extend(name.to_le_bytes());
            c.extend(20u16.to_le_bytes());
            c.extend(20u16.to_le_bytes());
            c.extend((attrs.len() as u16).to_le_bytes());
            c.extend([0u8; 6]);
            for (an, ty, data) in attrs.iter() {
                let an = sid(an);
                c.extend(u32::MAX.to_le_bytes());
                c.extend(an.to_le_bytes());
                c.extend(if *ty == 3 { text_id } else { u32::MAX }.to_le_bytes());
                c.extend(8u16.to_le_bytes());
                c.push(0);
                c.push(*ty);
                c.extend(if *ty == 3 { text_id } else { *data }.to_le_bytes());
            }
            chunks.push(c);
        }
        for _ in elems.iter() {
            let mut c = Vec::new();
            c.extend(0x0103u16.to_le_bytes());
            c.extend(16u16.to_le_bytes());
            c.extend(24u32.to_le_bytes());
            c.extend([0u8; 16]);
            chunks.push(c);
        }
        // the string pool (UTF-16)
        let mut data = Vec::new();
        let mut offs = Vec::new();
        for s in &strings {
            offs.push(data.len() as u32);
            let u: Vec<u16> = s.encode_utf16().collect();
            data.extend((u.len() as u16).to_le_bytes());
            for x in u {
                data.extend(x.to_le_bytes());
            }
            data.extend([0, 0]);
        }
        while data.len() % 4 != 0 {
            data.push(0);
        }
        let start = 28 + 4 * strings.len();
        let mut pool = Vec::new();
        pool.extend(0x0001u16.to_le_bytes());
        pool.extend(28u16.to_le_bytes());
        pool.extend(((start + data.len()) as u32).to_le_bytes());
        pool.extend((strings.len() as u32).to_le_bytes());
        pool.extend(0u32.to_le_bytes());
        pool.extend(0u32.to_le_bytes());
        pool.extend((start as u32).to_le_bytes());
        pool.extend(0u32.to_le_bytes());
        for o in offs {
            pool.extend(o.to_le_bytes());
        }
        pool.extend(data);
        let body: Vec<u8> = std::iter::once(pool).chain(chunks).flatten().collect();
        let mut out = Vec::new();
        out.extend(0x0003u16.to_le_bytes());
        out.extend(8u16.to_le_bytes());
        out.extend(((8 + body.len()) as u32).to_le_bytes());
        out.extend(body);
        out
    }

    pub(super) fn zip_of(path: &Path, members: &[(&str, &[u8])]) {
        let mut z = zip::ZipWriter::new(File::create(path).unwrap());
        for (name, data) in members {
            z.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            z.write_all(data).unwrap();
        }
        z.finish().unwrap();
    }

    #[test]
    fn reads_a_windows_package() {
        let dir = std::env::temp_dir().join(format!("rf-apppkg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("app.msix");
        let manifest = br#"<?xml version="1.0" encoding="utf-8"?>
<Package xmlns="http://schemas.microsoft.com/appx/manifest/foundation/windows10" xmlns:rescap="x">
  <Identity Name="Contoso.Hesap" Publisher="CN=Contoso &amp; Ortaklar, O=Contoso" Version="1.2.3.0" ProcessorArchitecture="x64" />
  <Properties><DisplayName>Hesap Makinesi</DisplayName><PublisherDisplayName>Contoso</PublisherDisplayName><Logo>Assets\StoreLogo.png</Logo></Properties>
  <Dependencies><TargetDeviceFamily Name="Windows.Desktop" MinVersion="10.0.17763.0" MaxVersionTested="10.0.22621.0" /></Dependencies>
  <Capabilities><rescap:Capability Name="runFullTrust" /><Capability Name="internetClient" /></Capabilities>
</Package>"#;
        zip_of(&p, &[("AppxManifest.xml", manifest), ("Assets/StoreLogo.scale-100.png", b"\x89PNG small"), ("Assets/StoreLogo.scale-200.png", b"\x89PNG big")]);
        let (k, i) = info(&p).unwrap();
        assert_eq!(k, Kind::Msix);
        assert_eq!(i.name.as_deref(), Some("Hesap Makinesi"));
        assert_eq!(i.id.as_deref(), Some("Contoso.Hesap"));
        assert_eq!(i.version.as_deref(), Some("1.2.3.0"));
        assert_eq!(i.publisher.as_deref(), Some("Contoso"));
        assert_eq!(i.arch, ["x64"]);
        assert_eq!(i.min_os.as_deref(), Some("Windows 10.0.17763.0"));
        assert_eq!(i.permissions, 2);
        assert_eq!(i.icon_png.as_deref(), Some(&b"\x89PNG big"[..]));
        assert_eq!(publisher_cn("CN=Contoso & Ortaklar, O=Contoso"), "Contoso & Ortaklar");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reads_plists_binary_and_xml() {
        let xml = br#"<?xml version="1.0"?><plist version="1.0"><dict>
<key>CFBundleIdentifier</key><string>com.ornek.uygulama</string>
<key>CFBundleIcons</key><dict><key>Ic</key><string>yok</string></dict>
<key>CFBundleShortVersionString</key><string>2.1</string>
<key>MinimumOSVersion</key><string>15.0</string></dict></plist>"#;
        let d = plist_strings(xml).unwrap();
        assert!(d.contains(&("CFBundleIdentifier".into(), "com.ornek.uygulama".into())));
        assert!(d.contains(&("MinimumOSVersion".into(), "15.0".into())));
        assert!(!d.iter().any(|(k, _)| k == "Ic"));
        // a binary plist: { "CFBundleName": "Örnek", "CFBundleVersion": "42" } built by hand
        let mut b = b"bplist00".to_vec();
        let mut offs = Vec::new();
        offs.push(b.len());
        b.extend([0xd2, 1, 2, 3, 4]); // dict of 2: keys 1,2 values 3,4
        offs.push(b.len());
        b.extend([0x5c]);
        b.extend(b"CFBundleName");
        offs.push(b.len());
        b.extend([0x5f, 0x10, 15]);
        b.extend(b"CFBundleVersion");
        offs.push(b.len());
        b.extend([0x65]);
        for u in "Örnek".encode_utf16() {
            b.extend(u.to_be_bytes());
        }
        offs.push(b.len());
        b.extend([0x52]);
        b.extend(b"42");
        let table = b.len();
        for o in &offs {
            b.push(*o as u8);
        }
        b.extend([0u8; 6]);
        b.extend([1, 1]);
        b.extend((offs.len() as u64).to_be_bytes());
        b.extend(0u64.to_be_bytes());
        b.extend((table as u64).to_be_bytes());
        let d = plist_strings(&b).unwrap();
        assert_eq!(d, [("CFBundleName".into(), "Örnek".into()), ("CFBundleVersion".into(), "42".into())]);
    }

    /// Prints what real packages say: `RF_PACKAGES="a.apk;b.msix" cargo test -p rf-win --lib real_packages -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_packages() {
        for p in std::env::var("RF_PACKAGES").unwrap_or_default().split(';').filter(|p| !p.is_empty()) {
            let r = info(Path::new(p)).map(|(k, mut i)| {
                // RF_ICON_OUT=folder: the icons are saved there to look at
                if let (Ok(dir), Some(png)) = (std::env::var("RF_ICON_OUT"), &i.icon_png) {
                    let name = Path::new(p).file_stem().unwrap().to_string_lossy().into_owned();
                    std::fs::write(Path::new(&dir).join(format!("{name}.png")), png).unwrap();
                }
                let icon = i.icon_png.take().map(|v| v.len());
                (k, i, icon)
            });
            println!("{p}
  {r:?}");
        }
    }

    #[test]
    fn unescapes_entities() {
        assert_eq!(unescape("A &amp; B &lt;x&gt; &#252;&#x131; &bogus; &"), "A & B <x> üı &bogus; &");
    }
}
