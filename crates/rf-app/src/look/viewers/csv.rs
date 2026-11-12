//! CSV / TSV as a table: the separator is detected (comma, semicolon — what Excel writes on a
//! Turkish system — or tab), quoted fields are parsed, the first row is the header. The first
//! 2 000 rows; the table scrolls both ways.

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::look::viewer::{Prepared, Probe, Viewer};

const EXTENSIONS: &[&str] = &["csv", "tsv"];
const ROWS_MAX: usize = 2000;
const READ_MAX: u64 = 16 << 20;

#[derive(Default)]
pub struct CsvViewer {
    table: Option<std::rc::Rc<Table>>,
    error: Option<String>,
}

struct Table {
    rows: Vec<Vec<String>>,
    /// Column widths in characters (capped).
    widths: Vec<usize>,
    /// Rows in the file beyond those shown.
    more: usize,
}

/// The separator that splits the first lines most consistently.
fn detect(sample: &str) -> char {
    let lines: Vec<&str> = sample.lines().take(20).filter(|l| !l.trim().is_empty()).collect();
    let mut best = (',', 0usize);
    for sep in ['\t', ';', ',', '|'] {
        let counts: Vec<usize> = lines.iter().map(|l| l.matches(sep).count()).collect();
        let first = counts.first().copied().unwrap_or(0);
        // the same count on every line scores; more columns wins ties
        if first > 0 && counts.iter().all(|&c| c == first) && first > best.1 {
            best = (sep, first);
        }
    }
    if best.1 > 0 {
        return best.0;
    }
    // ragged: the most frequent one
    ['\t', ';', ','].into_iter().max_by_key(|s| sample.matches(*s).count()).unwrap_or(',')
}

/// RFC 4180-style parsing: quotes, doubled quotes, separators and line breaks inside quotes.
fn parse(text: &str, sep: char, max_rows: usize) -> (Vec<Vec<String>>, usize) {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    let mut extra = 0usize;
    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    field.push('"');
                    chars.next();
                }
                '"' => quoted = false,
                _ => field.push(c),
            }
            continue;
        }
        match c {
            '"' if field.is_empty() => quoted = true,
            c if c == sep => row.push(std::mem::take(&mut field)),
            '\r' => {}
            '\n' => {
                row.push(std::mem::take(&mut field));
                if rows.len() < max_rows {
                    rows.push(std::mem::take(&mut row));
                } else {
                    row.clear();
                    extra += 1;
                }
            }
            _ => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        if rows.len() < max_rows {
            rows.push(row);
        } else {
            extra += 1;
        }
    }
    (rows, extra)
}

fn load(path: &std::path::Path) -> std::io::Result<Table> {
    use std::io::Read;
    let mut buf = Vec::new();
    std::fs::File::open(path)?.take(READ_MAX).read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    let text = text.trim_start_matches('\u{feff}');
    let sep = if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("tsv")) { '\t' } else {
        // (64 KB, cut back to a character boundary: a multi-byte letter may straddle it)
        let mut cut = text.len().min(64 << 10);
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        detect(&text[..cut])
    };
    let (rows, more) = parse(text, sep, ROWS_MAX);
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let mut widths = vec![3usize; cols];
    for r in rows.iter().take(500) {
        for (i, f) in r.iter().enumerate() {
            widths[i] = widths[i].max(f.chars().count().min(40));
        }
    }
    Ok(Table { rows, widths, more })
}

impl Viewer for CsvViewer {
    fn name(&self) -> &'static str {
        "csv"
    }
    fn priority(&self) -> i32 {
        -4
    }
    fn can_handle(&self, f: &Probe) -> bool {
        f.is_file() && EXTENSIONS.contains(&f.ext.as_str())
    }
    fn prepare(&mut self, f: &Probe, _cx: &mut App) -> Prepared {
        match load(&f.path) {
            Ok(t) => self.table = Some(std::rc::Rc::new(t)),
            Err(e) => self.error = Some(e.to_string()),
        }
        Prepared::new((1000.0, 700.0), super::file_name(&f.path))
    }
    fn render(&mut self, window: &mut Window, _cx: &mut App) -> AnyElement {
        let p = crate::theme::Palette::for_window(window);
        let Some(t) = &self.table else {
            return div().size_full().p_4().text_color(p.danger).child(self.error.clone().unwrap_or_default()).into_any_element();
        };
        let char_w = 8.6;
        let col_w: std::rc::Rc<Vec<f32>> = std::rc::Rc::new(t.widths.iter().map(|&w| w as f32 * char_w + 24.0).collect());
        let total: f32 = col_w.iter().sum::<f32>() + 48.0;
        // one row (only the rows on screen are built: the list below is virtual)
        let row = move |ri: usize, r: &[String], col_w: &[f32]| -> AnyElement {
            let header = ri == 0;
            let mut line = div()
                .flex()
                .flex_row()
                .h(px(26.))
                .items_center()
                .when(header, |d| d.bg(p.hover).font_weight(FontWeight::SEMIBOLD))
                .when(!header && ri % 2 == 0, |d| d.bg(p.tile_bg))
                .child(div().w(px(48.)).flex_shrink_0().px_2().text_right().text_color(p.text_tertiary).child(if header { String::new() } else { ri.to_string() }));
            for (ci, w) in col_w.iter().enumerate() {
                let v = r.get(ci).cloned().unwrap_or_default();
                line = line.child(
                    div()
                        .w(px(*w))
                        .flex_shrink_0()
                        .px_2()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .border_l_1()
                        .border_color(p.divider)
                        .child(v),
                );
            }
            line.into_any_element()
        };
        let header = t.rows.first().map(|r| row(0, r, &col_w));
        let body_rows = t.rows.len().saturating_sub(1);
        let n = body_rows + usize::from(t.more > 0);
        let (table, widths) = (t.clone(), col_w.clone());
        let list = uniform_list("dl-csv-rows", n, move |range, _window, _cx| {
            range
                .map(|i| {
                    if i < body_rows {
                        row(i + 1, &table.rows[i + 1], &widths)
                    } else {
                        div().h(px(26.)).px_3().flex().items_center().text_xs().text_color(p.text_secondary).child(format!("+{} {}", table.more, crate::t!("satır daha"))).into_any_element()
                    }
                })
                .collect()
        })
        .w(px(total))
        .flex_1()
        // a sideways wheel scrolls the table (the outer box), not the rows
        .map(|mut l| {
            l.style().restrict_scroll_to_axis = Some(true);
            l
        });
        // the header stays on top; the whole table scrolls sideways, the rows up and down
        div()
            .id("dl-csv")
            .size_full()
            .overflow_scroll()
            .bg(p.content_bg)
            .text_color(p.text)
            .text_sm()
            .font_family("Cascadia Mono")
            .child(div().flex().flex_col().w(px(total)).h_full().children(header).child(list))
            .into_any_element()
    }
    fn cleanup(&mut self) {
        self.table = None;
        self.error = None;
    }
}

#[cfg(test)]
mod tests {
    use super::{detect, parse};

    #[test]
    fn separators_and_quotes() {
        assert_eq!(detect("ad;soyad;yaş\nAli;Veli;3\n"), ';');
        assert_eq!(detect("a,b,c\n1,2,3\n"), ',');
        assert_eq!(detect("a\tb\n1\t2\n"), '\t');
        let (rows, more) = parse("ad,not\n\"Yılmaz, Ali\",\"satır\nkırık \"\"tırnak\"\"\"\nx,y\n", ',', 10);
        assert_eq!(more, 0);
        assert_eq!(rows[1], vec!["Yılmaz, Ali".to_string(), "satır\nkırık \"tırnak\"".to_string()]);
        assert_eq!(rows[2], vec!["x".to_string(), "y".to_string()]);
        let (rows, more) = parse("1\n2\n3\n4\n", ',', 2);
        assert_eq!((rows.len(), more), (2, 2));
    }
}
