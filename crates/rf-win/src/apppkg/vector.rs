//! Android drawables drawn to a picture: vector drawables and what usually holds them in a
//! launcher icon (adaptive icons, insets, layer lists, shapes, bitmaps), turned into one SVG for
//! resvg. Colours and gradients that live elsewhere in the package are looked up in
//! resources.arsc.

use std::collections::HashMap;
use std::fmt::Write as _;

use super::{Zip, arsc_values, axml_events, is_picture, member, res_id};

/// How deep references may chain (a colour naming a colour naming a file…).
const MAX_DEPTH: u8 = 8;

/// One element of a binary XML file.
struct Node {
    tag: String,
    attrs: HashMap<String, String>,
    kids: Vec<Node>,
}

impl Node {
    fn get(&self, k: &str) -> Option<&str> {
        self.attrs.get(k).map(String::as_str)
    }
    fn num(&self, k: &str) -> Option<f32> {
        self.get(k).and_then(number)
    }
}

/// The element tree of an Android binary XML file.
fn tree(bytes: &[u8]) -> Option<Node> {
    let mut stack: Vec<Node> = Vec::new();
    let mut root = None;
    axml_events(bytes, &mut |ev| match ev {
        Some((tag, attrs)) => stack.push(Node { tag: tag.to_string(), attrs: attrs.clone(), kids: Vec::new() }),
        None => {
            if let Some(n) = stack.pop() {
                match stack.last_mut() {
                    Some(parent) => parent.kids.push(n),
                    None => root = Some(n),
                }
            }
        }
    })?;
    root
}

/// The leading number of "12dp", "16.5", "-3px"…
fn number(s: &str) -> Option<f32> {
    let end = s.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+' || c == 'e' || c == 'E')).unwrap_or(s.len());
    s[..end].parse().ok()
}

/// A length: "16%" of `full`, or a plain number / dimension (the box is in dp).
fn length(s: &str, full: f32) -> Option<f32> {
    let v = number(s)?;
    Some(if s.contains('%') { v / 100.0 * full } else { v })
}

/// "#AARRGGBB", "#RRGGBB", "#ARGB", "#RGB" → (r, g, b, alpha 0..1).
fn color(s: &str) -> Option<(u8, u8, u8, f32)> {
    let h = s.strip_prefix('#')?;
    let v = u32::from_str_radix(h, 16).ok()?;
    let nib = |x: u32| ((x & 0xf) * 17) as u8;
    Some(match h.len() {
        8 => ((v >> 16) as u8, (v >> 8) as u8, v as u8, (v >> 24) as f32 / 255.0),
        6 => ((v >> 16) as u8, (v >> 8) as u8, v as u8, 1.0),
        4 => (nib(v >> 8), nib(v >> 4), nib(v), nib(v >> 12) as f32 / 255.0),
        3 => (nib(v >> 8), nib(v >> 4), nib(v), 1.0),
        _ => return None,
    })
}

fn hex(c: (u8, u8, u8, f32)) -> String {
    format!("#{:02x}{:02x}{:02x}", c.0, c.1, c.2)
}

/// What fills a shape: a colour (with its alpha) or a gradient defined in `<defs>`.
enum Paint {
    Color((u8, u8, u8, f32)),
    Url(String),
}

impl Paint {
    /// ` fill="…" fill-opacity="…"` (or stroke).
    fn attrs(&self, what: &str, alpha: f32) -> String {
        match self {
            Paint::Color(c) => format!(r#" {what}="{}" {what}-opacity="{}""#, hex(*c), c.3 * alpha),
            Paint::Url(id) => format!(r#" {what}="url(#{id})" {what}-opacity="{alpha}""#),
        }
    }
}

#[derive(Clone, Copy)]
struct Rect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

struct Ctx<'a> {
    zip: &'a mut Zip,
    arsc: &'a [u8],
    defs: String,
    next: u32,
}

impl Ctx<'_> {
    fn id(&mut self, prefix: &str) -> String {
        self.next += 1;
        format!("{prefix}{}", self.next)
    }

    /// The versions of a resource worth trying, best first: densest bitmaps, then the rest.
    fn values(&self, id: u32) -> Vec<super::ResValue> {
        let mut v = arsc_values(self.arsc, id);
        v.sort_by_key(|r| {
            let xml = r.string.as_deref().is_some_and(|s| s.ends_with(".xml"));
            (xml, std::cmp::Reverse(if r.density >= 0xfffe { 0 } else { r.density }))
        });
        v
    }

    /// A drawable named by an attribute value ("#ff…" or "@0x7f…"), drawn into `r`.
    fn drawable(&mut self, value: &str, r: Rect, stretch: bool, depth: u8) -> Option<String> {
        if depth > MAX_DEPTH {
            return None;
        }
        if let Some(c) = color(value) {
            return Some(rect_el(r, &Paint::Color(c), 0.0));
        }
        for v in self.values(res_id(value)?) {
            let found = match (v.dtype, v.string) {
                (0x1c..=0x1f, _) => Some(rect_el(r, &Paint::Color(argb(v.data)), 0.0)),
                (0x01, _) => self.drawable(&format!("@0x{:08x}", v.data), r, stretch, depth + 1),
                (_, Some(f)) if f.ends_with(".xml") => member(self.zip, &f).and_then(|b| tree(&b)).and_then(|n| self.node(&n, r, stretch, depth + 1)),
                (_, Some(f)) => member(self.zip, &f).filter(|b| is_picture(b)).and_then(|b| image_el(&b, r)),
                _ => None,
            };
            if found.is_some() {
                return found;
            }
        }
        None
    }

    /// A drawable XML element drawn into `r`.
    fn node(&mut self, n: &Node, r: Rect, stretch: bool, depth: u8) -> Option<String> {
        if depth > MAX_DEPTH {
            return None;
        }
        match n.tag.as_str() {
            "vector" => self.vector(n, r, stretch),
            "adaptive-icon" => {
                // layers are 108 dp; the middle 72 dp show, under the launcher's mask
                let clip = self.id("m");
                let full = Rect { x: 0.0, y: 0.0, w: 108.0, h: 108.0 };
                let mut s = format!(
                    r#"<svg x="{}" y="{}" width="{}" height="{}" viewBox="18 18 72 72"><clipPath id="{clip}"><rect x="18" y="18" width="72" height="72" rx="17"/></clipPath><g clip-path="url(#{clip})">"#,
                    r.x, r.y, r.w, r.h
                );
                for layer in ["background", "foreground"] {
                    let Some(l) = n.kids.iter().find(|k| k.tag == layer) else { continue };
                    let drawn = match l.get("drawable") {
                        Some(d) => self.drawable(d, full, true, depth + 1),
                        None => l.kids.first().and_then(|k| self.node(k, full, true, depth + 1)),
                    };
                    s += &drawn.unwrap_or_default();
                }
                s += "</g></svg>";
                Some(s)
            }
            "inset" => {
                let all = n.get("inset");
                let side = |k: &str, full: f32| n.get(k).or(all).and_then(|v| length(v, full)).unwrap_or(0.0);
                let (l, t, rt, b) = (side("insetLeft", r.w), side("insetTop", r.h), side("insetRight", r.w), side("insetBottom", r.h));
                let inner = Rect { x: r.x + l, y: r.y + t, w: (r.w - l - rt).max(0.0), h: (r.h - t - b).max(0.0) };
                match n.get("drawable") {
                    Some(d) => self.drawable(d, inner, stretch, depth + 1),
                    None => n.kids.first().and_then(|k| self.node(k, inner, stretch, depth + 1)),
                }
            }
            "layer-list" => {
                let mut s = String::new();
                for item in n.kids.iter().filter(|k| k.tag == "item") {
                    let side = |k: &str, full: f32| item.get(k).and_then(|v| length(v, full)).unwrap_or(0.0);
                    let (l, t, rt, b) = (side("left", r.w), side("top", r.h), side("right", r.w), side("bottom", r.h));
                    let inner = Rect { x: r.x + l, y: r.y + t, w: (r.w - l - rt).max(0.0), h: (r.h - t - b).max(0.0) };
                    let drawn = match item.get("drawable") {
                        Some(d) => self.drawable(d, inner, stretch, depth + 1),
                        None => item.kids.first().and_then(|k| self.node(k, inner, stretch, depth + 1)),
                    };
                    s += &drawn.unwrap_or_default();
                }
                Some(s)
            }
            "shape" => self.shape(n, r),
            "bitmap" | "nine-patch" => self.drawable(n.get("src")?, r, stretch, depth + 1),
            "selector" | "level-list" | "animated-selector" => {
                // the plain state: the last item (the one without conditions, by convention)
                let item = n.kids.iter().rev().find(|k| k.tag == "item")?;
                match item.get("drawable") {
                    Some(d) => self.drawable(d, r, stretch, depth + 1),
                    None => item.kids.first().and_then(|k| self.node(k, r, stretch, depth + 1)),
                }
            }
            "color" => {
                let p = self.paint(n.get("color")?, r, depth + 1)?;
                Some(rect_el(r, &p, 0.0))
            }
            _ => None,
        }
    }

    /// A colour or gradient named by an attribute value; gradients in `space` (the user space
    /// of what they fill).
    fn paint(&mut self, value: &str, space: Rect, depth: u8) -> Option<Paint> {
        if depth > MAX_DEPTH {
            return None;
        }
        if let Some(c) = color(value) {
            return Some(Paint::Color(c));
        }
        for v in self.values(res_id(value)?) {
            let found = match (v.dtype, v.string) {
                (0x1c..=0x1f, _) => Some(Paint::Color(argb(v.data))),
                (0x01, _) => self.paint(&format!("@0x{:08x}", v.data), space, depth + 1),
                (_, Some(f)) if f.ends_with(".xml") => {
                    let n = member(self.zip, &f).and_then(|b| tree(&b));
                    match n {
                        Some(n) if n.tag == "gradient" => self.gradient(&n, None),
                        // a colour state list: its plain state (the last item)
                        Some(n) if n.tag == "selector" => {
                            let item = n.kids.iter().rev().find(|k| k.tag == "item")?;
                            let alpha = item.num("alpha").unwrap_or(1.0);
                            match self.paint(item.get("color")?, space, depth + 1)? {
                                Paint::Color(c) => Some(Paint::Color((c.0, c.1, c.2, c.3 * alpha))),
                                p => Some(p),
                            }
                        }
                        _ => None,
                    }
                }
                _ => None,
            };
            if found.is_some() {
                return found;
            }
        }
        None
    }

    /// A `<gradient>` as an SVG gradient in `<defs>`. `shape` is the box of a shape drawable
    /// (whose gradient runs by angle across it); vector gradients give coordinates instead.
    fn gradient(&mut self, n: &Node, shape: Option<Rect>) -> Option<Paint> {
        let col = |k: &str| n.get(k).and_then(color);
        let mut stops: Vec<(f32, (u8, u8, u8, f32))> = n
            .kids
            .iter()
            .filter(|k| k.tag == "item")
            .filter_map(|k| Some((k.num("offset").unwrap_or(0.0), k.get("color").and_then(color)?)))
            .collect();
        if stops.is_empty() {
            stops.push((0.0, col("startColor")?));
            if let Some(c) = col("centerColor") {
                stops.push((0.5, c));
            }
            stops.push((1.0, col("endColor").unwrap_or(stops[0].1)));
        }
        let id = self.id("g");
        let kind = n.num("type").unwrap_or(0.0) as i32;
        let spread = match n.get("tileMode") {
            Some("2") => "reflect",
            Some("1") => "repeat",
            _ => "pad",
        };
        let mut s = match (kind, shape) {
            (1, _) => {
                let (cx, cy) = match shape {
                    Some(r) => (r.x + r.w * n.num("centerX").unwrap_or(0.5), r.y + r.h * n.num("centerY").unwrap_or(0.5)),
                    None => (n.num("centerX").unwrap_or(0.0), n.num("centerY").unwrap_or(0.0)),
                };
                let rad = n.get("gradientRadius").and_then(|v| length(v, shape.map_or(1.0, |r| r.w.min(r.h)))).unwrap_or(1.0);
                format!(r#"<radialGradient id="{id}" gradientUnits="userSpaceOnUse" cx="{cx}" cy="{cy}" r="{rad}" spreadMethod="{spread}">"#)
            }
            (_, Some(r)) => {
                // a shape's angle: 0 runs left → right, 90 bottom → top
                let a = n.num("angle").unwrap_or(0.0).to_radians();
                let (dx, dy) = (a.cos() * r.w / 2.0, -a.sin() * r.h / 2.0);
                let (cx, cy) = (r.x + r.w / 2.0, r.y + r.h / 2.0);
                format!(r#"<linearGradient id="{id}" gradientUnits="userSpaceOnUse" x1="{}" y1="{}" x2="{}" y2="{}" spreadMethod="{spread}">"#, cx - dx, cy - dy, cx + dx, cy + dy)
            }
            // (a sweep gradient is drawn as a linear one)
            (_, None) => {
                let g = |k: &str| n.num(k).unwrap_or(0.0);
                format!(
                    r#"<linearGradient id="{id}" gradientUnits="userSpaceOnUse" x1="{}" y1="{}" x2="{}" y2="{}" spreadMethod="{spread}">"#,
                    g("startX"),
                    g("startY"),
                    g("endX"),
                    g("endY")
                )
            }
        };
        for (o, c) in stops {
            let _ = write!(s, r#"<stop offset="{}" stop-color="{}" stop-opacity="{}"/>"#, o.clamp(0.0, 1.0), hex(c), c.3);
        }
        s += if kind == 1 { "</radialGradient>" } else { "</linearGradient>" };
        self.defs += &s;
        Some(Paint::Url(id))
    }

    /// A `<shape>`: rectangle (rounded) or oval, filled and outlined.
    fn shape(&mut self, n: &Node, r: Rect) -> Option<String> {
        let oval = n.get("shape") == Some("1");
        let corners = n.kids.iter().find(|k| k.tag == "corners").and_then(|k| k.get("radius")).and_then(number).unwrap_or(0.0);
        let fill = match n.kids.iter().find(|k| k.tag == "solid" || k.tag == "gradient") {
            Some(k) if k.tag == "gradient" => self.gradient(k, Some(r)),
            Some(k) => k.get("color").and_then(|c| self.paint(c, r, 1)),
            None => None,
        };
        let stroke = n.kids.iter().find(|k| k.tag == "stroke").and_then(|k| {
            let w = k.get("width").and_then(number).unwrap_or(1.0);
            let p = self.paint(k.get("color")?, r, 1)?;
            Some(format!(r#"{} stroke-width="{w}""#, p.attrs("stroke", 1.0)))
        });
        let fill = fill.map_or(r#" fill="none""#.to_string(), |p| p.attrs("fill", 1.0));
        let stroke = stroke.unwrap_or_default();
        Some(if oval {
            format!(r#"<ellipse cx="{}" cy="{}" rx="{}" ry="{}"{fill}{stroke}/>"#, r.x + r.w / 2.0, r.y + r.h / 2.0, r.w / 2.0, r.h / 2.0)
        } else {
            format!(r#"<rect x="{}" y="{}" width="{}" height="{}" rx="{corners}"{fill}{stroke}/>"#, r.x, r.y, r.w, r.h)
        })
    }

    /// A `<vector>` drawn into `r` (stretched to it inside an adaptive icon, as Android does).
    fn vector(&mut self, n: &Node, r: Rect, stretch: bool) -> Option<String> {
        let vw = n.num("viewportWidth").filter(|v| *v > 0.0)?;
        let vh = n.num("viewportHeight").filter(|v| *v > 0.0)?;
        let tint = n.get("tint").and_then(|t| self.paint(t, r, 1));
        let alpha = n.num("alpha").unwrap_or(1.0);
        let mut s = format!(
            r#"<svg x="{}" y="{}" width="{}" height="{}" viewBox="0 0 {vw} {vh}" preserveAspectRatio="{}" opacity="{alpha}">"#,
            r.x,
            r.y,
            r.w,
            r.h,
            if stretch { "none" } else { "xMidYMid meet" }
        );
        let space = Rect { x: 0.0, y: 0.0, w: vw, h: vh };
        self.children(n, &mut s, space, tint.as_ref());
        s += "</svg>";
        Some(s)
    }

    /// A vector's or group's children; a `<clip-path>` clips the siblings after it.
    fn children(&mut self, n: &Node, s: &mut String, space: Rect, tint: Option<&Paint>) {
        let mut open = 0;
        for k in &n.kids {
            match k.tag.as_str() {
                "group" => {
                    let g = |a: &str, d: f32| k.num(a).unwrap_or(d);
                    let (px, py) = (g("pivotX", 0.0), g("pivotY", 0.0));
                    let _ = write!(
                        s,
                        r#"<g transform="translate({} {}) rotate({}) scale({} {}) translate({} {})">"#,
                        g("translateX", 0.0) + px,
                        g("translateY", 0.0) + py,
                        g("rotation", 0.0),
                        g("scaleX", 1.0),
                        g("scaleY", 1.0),
                        -px,
                        -py
                    );
                    self.children(k, s, space, tint);
                    s.push_str("</g>");
                }
                "clip-path" => {
                    if let Some(d) = k.get("pathData") {
                        let id = self.id("c");
                        let _ = write!(self.defs, r#"<clipPath id="{id}"><path d="{}"/></clipPath>"#, esc(d));
                        let _ = write!(s, r#"<g clip-path="url(#{id})">"#);
                        open += 1;
                    }
                }
                "path" => {
                    let Some(d) = k.get("pathData") else { continue };
                    let mut el = format!(r#"<path d="{}""#, esc(d));
                    let fill = k.get("fillColor").and_then(|c| self.paint(c, space, 1));
                    match (fill, tint) {
                        (Some(_), Some(t)) => el += &t.attrs("fill", k.num("fillAlpha").unwrap_or(1.0)),
                        (Some(p), None) => el += &p.attrs("fill", k.num("fillAlpha").unwrap_or(1.0)),
                        (None, _) => el += r#" fill="none""#,
                    }
                    if k.get("fillType").is_some_and(|t| t == "1" || t == "evenOdd") {
                        el += r#" fill-rule="evenodd""#;
                    }
                    let stroke = k.get("strokeColor").and_then(|c| self.paint(c, space, 1));
                    let width = k.num("strokeWidth").unwrap_or(0.0);
                    if let (Some(p), true) = (stroke, width > 0.0) {
                        el += &tint.unwrap_or(&p).attrs("stroke", k.num("strokeAlpha").unwrap_or(1.0));
                        let cap = match k.get("strokeLineCap") {
                            Some("1" | "round") => "round",
                            Some("2" | "square") => "square",
                            _ => "butt",
                        };
                        let join = match k.get("strokeLineJoin") {
                            Some("1" | "round") => "round",
                            Some("2" | "bevel") => "bevel",
                            _ => "miter",
                        };
                        let _ = write!(
                            el,
                            r#" stroke-width="{width}" stroke-linecap="{cap}" stroke-linejoin="{join}" stroke-miterlimit="{}""#,
                            k.num("strokeMiterLimit").unwrap_or(4.0)
                        );
                    }
                    el += "/>";
                    s.push_str(&el);
                }
                _ => {}
            }
        }
        for _ in 0..open {
            s.push_str("</g>");
        }
    }
}

/// A compiled colour value (ARGB; the RGB forms have no alpha).
fn argb(v: u32) -> (u8, u8, u8, f32) {
    ((v >> 16) as u8, (v >> 8) as u8, v as u8, (v >> 24) as f32 / 255.0)
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('"', "&quot;")
}

fn rect_el(r: Rect, p: &Paint, rx: f32) -> String {
    format!(r#"<rect x="{}" y="{}" width="{}" height="{}" rx="{rx}"{}/>"#, r.x, r.y, r.w, r.h, p.attrs("fill", 1.0))
}

/// A PNG / WebP layer, stretched to `r` (WebP turned into PNG for the renderer).
fn image_el(bytes: &[u8], r: Rect) -> Option<String> {
    let png = if bytes.starts_with(b"\x89PNG") {
        bytes.to_vec()
    } else {
        let img = image::load_from_memory(bytes).ok()?;
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).ok()?;
        out.into_inner()
    };
    Some(format!(
        r#"<image x="{}" y="{}" width="{}" height="{}" preserveAspectRatio="none" href="data:image/png;base64,{}"/>"#,
        r.x,
        r.y,
        r.w,
        r.h,
        base64(&png)
    ))
}

fn base64(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for k in 0..4 {
            if k <= c.len() {
                s.push(T[(n >> (18 - 6 * k) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

/// The SVG for a drawable XML file (an icon resource), `size` pixels square; None when it holds
/// nothing we can draw.
fn svg_of(zip: &mut Zip, arsc: &[u8], xml: &[u8], size: u32) -> Option<String> {
    let root = tree(xml)?;
    let mut c = Ctx { zip, arsc, defs: String::new(), next: 0 };
    let s = size as f32;
    let body = c.node(&root, Rect { x: 0.0, y: 0.0, w: s, h: s }, false, 0)?;
    if body.is_empty() {
        return None;
    }
    Some(format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{size}" height="{size}" viewBox="0 0 {size} {size}"><defs>{}</defs>{body}</svg>"#,
        c.defs
    ))
}

/// A drawable XML file (vector, adaptive icon…) drawn as a `size`-pixel PNG.
pub(super) fn render(zip: &mut Zip, arsc: &[u8], xml: &[u8], size: u32) -> Option<Vec<u8>> {
    let svg = svg_of(zip, arsc, xml, size)?;
    rasterize(&svg, size)
}

fn rasterize(svg: &str, size: u32) -> Option<Vec<u8>> {
    use resvg::{tiny_skia, usvg};
    let tree = usvg::Tree::from_str(svg, &usvg::Options::default()).ok()?;
    let mut pix = tiny_skia::Pixmap::new(size, size)?;
    resvg::render(&tree, tiny_skia::Transform::identity(), &mut pix.as_mut());
    let mut data = pix.take();
    if data.chunks_exact(4).all(|p| p[3] == 0) {
        return None;
    }
    // premultiplied → straight alpha
    for px in data.chunks_exact_mut(4) {
        let a = px[3] as u32;
        if a > 0 && a < 255 {
            for c in &mut px[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    let img = image::RgbaImage::from_raw(size, size, data)?;
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_lengths_and_base64() {
        assert_eq!(color("#80ff0000"), Some((255, 0, 0, 128.0 / 255.0)));
        assert_eq!(color("#3ddc84"), Some((0x3d, 0xdc, 0x84, 1.0)));
        assert_eq!(color("#f0a"), Some((255, 0, 170, 1.0)));
        assert_eq!(length("16%", 108.0), Some(16.0 / 100.0 * 108.0));
        assert_eq!(length("12dp", 108.0), Some(12.0));
        assert_eq!(number("-1.5px"), Some(-1.5));
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b"M"), "TQ==");
    }

    /// A hand-built binary XML: `<vector viewportWidth=24 viewportHeight=24><path pathData=…
    /// fillColor=#ff0000ff/></vector>` turns into an SVG that draws.
    #[test]
    fn draws_a_vector() {
        let xml = super::super::tests::axml_doc(&[
            ("vector", &[("viewportWidth", 0x04, 24f32.to_bits()), ("viewportHeight", 0x04, 24f32.to_bits())][..]),
            ("path", &[("pathData", 0x03, u32::MAX), ("fillColor", 0x1c, 0xff0000ff)][..]),
        ], "M0,0h24v24h-24z");
        let n = tree(&xml).unwrap();
        assert_eq!(n.tag, "vector");
        assert_eq!(n.kids[0].get("fillColor"), Some("#ff0000ff"));
        let dir = std::env::temp_dir().join(format!("rf-vec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("x.zip");
        super::super::tests::zip_of(&p, &[("a", b"a")]);
        let mut zip = zip::ZipArchive::new(std::io::BufReader::new(std::fs::File::open(&p).unwrap())).unwrap();
        let svg = svg_of(&mut zip, &[], &xml, 48).unwrap();
        assert!(svg.contains(r##"fill="#0000ff""##), "{svg}");
        let png = rasterize(&svg, 48).unwrap();
        let img = image::load_from_memory(&png).unwrap().into_rgba8();
        assert_eq!(img.get_pixel(24, 24).0, [0, 0, 255, 255]);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
