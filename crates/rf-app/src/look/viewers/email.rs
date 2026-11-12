//! E-mail files (.eml): subject, sender, recipients, date, the attachments and the message text
//! (the plain part, or text taken from the HTML part).

use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::*;

use crate::look::viewer::{Prepared, Probe, Viewer};
use crate::t;

const READ_MAX: u64 = 32 << 20;

#[derive(Default)]
pub struct EmailViewer {
    mail: Option<Mail>,
    error: Option<String>,
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct Mail {
    pub subject: String,
    pub from: String,
    pub to: String,
    pub date: String,
    pub body: String,
    /// (name, size in bytes)
    pub attachments: Vec<(String, usize)>,
}

fn addresses(a: Option<mail_parser::AddressList>) -> String {
    let Some(a) = a else { return String::new() };
    a.mailboxes()
        .map(|x| match (x.name(), x.address()) {
            (Some(n), Some(e)) if !n.is_empty() => format!("{n} <{e}>"),
            (_, Some(e)) => e.to_string(),
            (Some(n), None) => n.to_string(),
            _ => String::new(),
        })
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) fn parse(bytes: &[u8]) -> Option<Mail> {
    let m = mail_parser::MessageParser::default().parse(bytes)?;
    let date = m.date().map(|d| {
        let ms = d.to_timestamp() * 1000;
        rf_core::format::datetime_full(ms)
    });
    Some(Mail {
        subject: m.subject().unwrap_or_default().to_string(),
        from: addresses(m.from()),
        to: addresses(m.to()),
        date: date.unwrap_or_default(),
        body: m.body_text(0).map(|b| b.trim().to_string()).unwrap_or_default(),
        attachments: m.attachments().map(|a| (a.attachment_name().unwrap_or("?").to_string(), a.decoded_len())).collect(),
    })
}

impl Viewer for EmailViewer {
    fn name(&self) -> &'static str {
        "email"
    }
    fn priority(&self) -> i32 {
        -4
    }
    fn can_handle(&self, f: &Probe) -> bool {
        f.is_file() && f.ext == "eml"
    }
    fn prepare(&mut self, f: &Probe, _cx: &mut App) -> Prepared {
        use std::io::Read;
        let mut buf = Vec::new();
        match std::fs::File::open(&f.path).and_then(|file| file.take(READ_MAX).read_to_end(&mut buf)) {
            Ok(_) => match parse(&buf) {
                Some(m) => self.mail = Some(m),
                None => self.error = Some(t!("E-posta okunamadı").to_string()),
            },
            Err(e) => self.error = Some(e.to_string()),
        }
        Prepared::new((860.0, 760.0), super::file_name(&f.path))
    }
    fn render(&mut self, window: &mut Window, _cx: &mut App) -> AnyElement {
        let p = crate::theme::Palette::for_window(window);
        let Some(m) = &self.mail else {
            return div().size_full().p_4().text_color(p.danger).child(self.error.clone().unwrap_or_default()).into_any_element();
        };
        let field = |label: &'static str, value: &str| {
            h_flex()
                .gap_3()
                .text_sm()
                .child(div().w(px(64.)).flex_shrink_0().text_color(p.text_tertiary).child(label))
                .child(div().min_w_0().text_color(p.text_secondary).child(value.to_string()))
        };
        let mut head = v_flex()
            .gap_1p5()
            .px_6()
            .pt_5()
            .pb_4()
            .border_b_1()
            .border_color(p.divider)
            .child(div().text_xl().font_weight(FontWeight::SEMIBOLD).text_color(p.text).child(if m.subject.is_empty() { t!("(konu yok)").to_string() } else { m.subject.clone() }))
            .child(div().h(px(4.)))
            .child(field(t!("Kimden"), &m.from))
            .child(field(t!("Kime"), &m.to));
        if !m.date.is_empty() {
            head = head.child(field(t!("Tarih"), &m.date));
        }
        if !m.attachments.is_empty() {
            let mut chips = h_flex().gap_2().flex_wrap().pt_2();
            for (name, size) in &m.attachments {
                chips = chips.child(
                    h_flex()
                        .gap_2()
                        .px_3()
                        .py_1()
                        .rounded(crate::theme::RADIUS_SM)
                        .bg(p.hover)
                        .text_sm()
                        .child(div().text_color(p.text).child(name.clone()))
                        .child(div().text_color(p.text_tertiary).child(rf_core::format::size(*size as u64))),
                );
            }
            head = head.child(chips);
        }
        v_flex()
            .size_full()
            .bg(p.content_bg)
            .child(head)
            .child(
                div()
                    .id("dl-email-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_6()
                    .py_4()
                    .text_color(p.text)
                    .text_sm()
                    .line_height(px(22.))
                    .whitespace_normal()
                    .child(m.body.clone()),
            )
            .into_any_element()
    }
    fn cleanup(&mut self) {
        self.mail = None;
        self.error = None;
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_a_multipart_mail_with_an_attachment() {
        let raw = "From: =?UTF-8?Q?=C3=96zer_G=C3=BCzel?= <ozer@example.com>\r\n\
To: Ali <ali@example.com>, veli@example.com\r\n\
Subject: =?UTF-8?B?VGVzdCDDp8O2emfDvA==?=\r\n\
Date: Sun, 08 Nov 2026 10:30:00 +0300\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"b1\"\r\n\
\r\n\
--b1\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
\r\n\
Merhaba, ekte rapor var.\r\n\
--b1\r\n\
Content-Type: application/pdf; name=\"rapor.pdf\"\r\n\
Content-Disposition: attachment; filename=\"rapor.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
JVBERi0xLjQK\r\n\
--b1--\r\n";
        let m = super::parse(raw.as_bytes()).expect("parses");
        assert_eq!(m.subject, "Test çözgü");
        assert_eq!(m.from, "Özer Güzel <ozer@example.com>");
        assert_eq!(m.to, "Ali <ali@example.com>, veli@example.com");
        assert!(m.body.starts_with("Merhaba, ekte rapor var."));
        assert_eq!(m.attachments, vec![("rapor.pdf".to_string(), 9)]);
        assert!(!m.date.is_empty());
    }
}
