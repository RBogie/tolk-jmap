//! A body the device sends (`<airsyncbase:Body>`, MS-ASAIRS 2.2.2.9), read
//! by its `Type` (MS-ASAIRS 2.2.2.22.1): 1 plain text, 2 HTML, 3 RTF, 4 MIME.
//! Each collection says which types it can store; a body of another type is
//! not read as text but reported, so the Add or Change can be refused (see
//! `UNSUPPORTED_FIELDS`). A MIME body is read for its text and HTML.

use std::borrow::Cow;

use mail_parser::{Message, MessageParser};

pub use super::email::body_type::{HTML, MIME, PLAIN};
use crate::wbxml::{Element, cp::airsyncbase};

/// The `<Body>` in a device's `ApplicationData` or `<Exception>`.
pub struct Body<'a> {
    body_type: Option<u8>,
    data: &'a [u8],
}

/// What a body holds, for the types Tolk reads.
pub enum Content<'a> {
    Text(Cow<'a, str>),
    Html(Cow<'a, str>),
    Mime(&'a [u8]),
}

/// The body in `parent`, if the device sent one.
pub fn of(parent: &Element) -> Option<Body<'_>> {
    let body = parent.find(airsyncbase::Body)?;
    // Data is a string, or opaque bytes in WBXML.
    let data = body
        .find(airsyncbase::Data)
        .and_then(|d| d.text().map(str::as_bytes).or(d.opaque()))
        .unwrap_or_default();
    Some(Body {
        body_type: body
            .child_text(airsyncbase::Type)
            .and_then(|t| t.trim().parse().ok()),
        data,
    })
}

/// Why the body in `parent` cannot be stored where only the `allowed` types
/// can; `None` when it can, or when it is empty (nothing would be lost).
pub fn unsupported(parent: &Element, allowed: &[u8]) -> Option<&'static str> {
    let body = of(parent)?;
    if body.is_empty() {
        return None;
    }
    match body.body_type {
        Some(t) if allowed.contains(&t) => None,
        Some(PLAIN) => Some("Body of type 1 (plain text)"),
        Some(HTML) => Some("Body of type 2 (HTML)"),
        Some(3) => Some("Body of type 3 (RTF)"),
        Some(MIME) => Some("Body of type 4 (MIME)"),
        Some(_) => Some("Body of an unknown type"),
        None => Some("Body without a type"),
    }
}

/// Why the body in `parent` cannot be stored where there is no place for
/// files: a MIME body whose message carries attachments.
pub fn unsupported_files(parent: &Element) -> Option<&'static str> {
    let body = of(parent)?;
    let Some(Content::Mime(raw)) = body.content(&[MIME]) else {
        return None;
    };
    MessageParser::default()
        .parse(raw)
        .is_some_and(|m| m.attachments().len() > 0)
        .then_some("Body of type 4 (MIME) with attachments")
}

/// The text and HTML of a message the device sent. The parser makes up a
/// missing HTML body from the text; that one is left out.
pub fn mime_bodies(mime: &Message) -> (String, Option<String>) {
    let text = mime.body_text(0).unwrap_or_default().into_owned();
    let html = mime
        .html_body()
        .any(|p| p.is_content_type("text", "html"))
        .then(|| mime.body_html(0).map(|h| h.into_owned()))
        .flatten();
    (text, html)
}

impl<'a> Body<'a> {
    /// Whether the body has no content (devices send an empty one for a
    /// body they did not load).
    pub fn is_empty(&self) -> bool {
        self.data.trim_ascii().is_empty()
    }

    /// The content, for the types in `allowed`; `None` for another type,
    /// which [`unsupported`] reports.
    pub fn content(&self, allowed: &[u8]) -> Option<Content<'a>> {
        let t = self.body_type.filter(|t| allowed.contains(t))?;
        Some(match t {
            PLAIN => Content::Text(String::from_utf8_lossy(self.data)),
            HTML => Content::Html(String::from_utf8_lossy(self.data)),
            MIME => Content::Mime(self.data),
            _ => return None,
        })
    }

    /// The content as plain text, HTML converted, for the types in
    /// `allowed`; a message's text part.
    pub fn text(&self, allowed: &[u8]) -> Option<String> {
        match self.content(allowed)? {
            Content::Text(text) => Some(text.into_owned()),
            Content::Html(html) => Some(super::email::html_to_text(&html)),
            Content::Mime(raw) => Some(mime_bodies(&MessageParser::default().parse(raw)?).0),
        }
    }

    /// The content as text or HTML (`true`), for the types in `allowed`; a
    /// message's HTML part, or else its text.
    pub fn text_or_html(&self, allowed: &[u8]) -> Option<(String, bool)> {
        Some(match self.content(allowed)? {
            Content::Text(text) => (text.into_owned(), false),
            Content::Html(html) => (html.into_owned(), true),
            Content::Mime(raw) => match mime_bodies(&MessageParser::default().parse(raw)?) {
                (_, Some(html)) => (html, true),
                (text, None) => (text, false),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(body_type: Option<&str>, content: Element) -> Element {
        let mut body = Element::new(airsyncbase::Body);
        if let Some(t) = body_type {
            body.push(Element::with_text(airsyncbase::Type, t));
        }
        Element::new(crate::wbxml::cp::airsync::ApplicationData).child(body.child(content))
    }

    #[test]
    fn reads_bodies_by_type() {
        let text = data(Some("1"), Element::with_text(airsyncbase::Data, "Hi"));
        assert!(
            matches!(of(&text).unwrap().content(&[PLAIN]), Some(Content::Text(t)) if t == "Hi")
        );
        let html = data(
            Some("2"),
            Element::with_opaque(airsyncbase::Data, b"<p>Hi</p>".to_vec()),
        );
        assert!(
            matches!(of(&html).unwrap().content(&[PLAIN, HTML]), Some(Content::Html(h)) if h == "<p>Hi</p>")
        );
        assert_eq!(
            of(&html).unwrap().text(&[PLAIN, HTML]).unwrap().trim(),
            "Hi"
        );
        assert!(of(&html).unwrap().content(&[PLAIN]).is_none());
    }

    #[test]
    fn reads_a_message_for_its_text_and_html() {
        let message = "Content-Type: text/html\r\n\r\n<p>Met at <b>FOSDEM</b></p>\r\n";
        let mime = data(Some("4"), Element::with_text(airsyncbase::Data, message));
        let body = of(&mime).unwrap();
        assert_eq!(body.text(&[MIME]).unwrap().trim(), "Met at FOSDEM");
        let (html, is_html) = body.text_or_html(&[MIME]).unwrap();
        assert!(is_html && html.contains("<b>FOSDEM</b>"), "{html}");
    }

    #[test]
    fn reports_types_that_cannot_be_stored() {
        let rtf = data(Some("3"), Element::with_text(airsyncbase::Data, "e1xydGYx"));
        assert_eq!(
            unsupported(&rtf, &[PLAIN, HTML]),
            Some("Body of type 3 (RTF)")
        );
        let mime = data(
            Some("4"),
            Element::with_text(airsyncbase::Data, "Subject: x"),
        );
        assert_eq!(
            unsupported(&mime, &[PLAIN, HTML]),
            Some("Body of type 4 (MIME)")
        );
        assert_eq!(unsupported(&mime, &[PLAIN, HTML, MIME]), None);
        assert_eq!(unsupported_files(&mime), None);
        let untyped = data(None, Element::with_text(airsyncbase::Data, "Hi"));
        assert_eq!(unsupported(&untyped, &[PLAIN]), Some("Body without a type"));
        // An empty body loses nothing.
        let empty = data(Some("3"), Element::with_text(airsyncbase::Data, " "));
        assert_eq!(unsupported(&empty, &[PLAIN]), None);
        let none = Element::new(crate::wbxml::cp::airsync::ApplicationData);
        assert_eq!(unsupported(&none, &[PLAIN]), None);
    }
}
