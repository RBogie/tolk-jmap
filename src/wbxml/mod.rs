//! WBXML 1.3 as used by Exchange ActiveSync (MS-ASWBXML).
//!
//! EAS uses a small subset: no attributes, no processing instructions, UTF-8
//! only, and an (almost always) empty string table. Tags are identified by
//! code page plus token; [`cp`] has the named tags for every page we use.

pub mod cp;
mod decode;
mod encode;
#[cfg(test)]
mod proptests;
mod xml;

use std::fmt;

pub use decode::{DecodeError, decode};
pub use encode::{Chunk, encode, encode_parts};
pub use xml::to_xml;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Tag {
    pub page: u8,
    pub token: u8,
}

impl Tag {
    pub fn name(self) -> Option<&'static str> {
        cp::page(self.page)?
            .tags
            .iter()
            .find(|(token, _)| *token == self.token)
            .map(|(_, name)| *name)
    }

    pub fn namespace(self) -> Option<&'static str> {
        cp::page(self.page).map(|p| p.namespace)
    }
}

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.namespace() {
            Some(ns) => write!(f, "{ns}:")?,
            None => write!(f, "Page{}:", self.page)?,
        }
        match self.name() {
            Some(name) => write!(f, "{name}"),
            None => write!(f, "0x{:02X}", self.token),
        }
    }
}

impl fmt::Debug for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
    Element(Element),
    Text(String),
    Opaque(Vec<u8>),
    /// Text supplied while the response is sent (the n-th stream of the
    /// response, see [`encode_parts`]); only ever encoded, never decoded.
    Stream(usize),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Element {
    pub tag: Tag,
    pub children: Vec<Node>,
}

impl Element {
    /// The element's structure without its content, for diagnosing what a
    /// device rejects: tags, text lengths in bytes, and which text is not
    /// a plain inline string.
    pub fn shape(&self) -> String {
        let name = self.tag.name().unwrap_or("?");
        let parts: Vec<String> = self
            .children
            .iter()
            .map(|child| match child {
                Node::Element(e) => e.shape(),
                Node::Text(t) if t.contains('\0') => format!("text-with-NUL:{}", t.len()),
                Node::Text(t)
                    if t.chars()
                        .any(|c| c.is_control() && !matches!(c, '\r' | '\n' | '\t')) =>
                {
                    format!("text-with-control:{}", t.len())
                }
                Node::Text(t) => t.len().to_string(),
                Node::Opaque(d) => format!("opaque:{}", d.len()),
                Node::Stream(_) => "stream".to_owned(),
            })
            .collect();
        match parts.as_slice() {
            [] => name.to_owned(),
            [only] if !only.contains('(') && !only.contains('[') => format!("{name}({only})"),
            _ => format!("{name}[{}]", parts.join(" ")),
        }
    }

    pub fn new(tag: Tag) -> Self {
        Self {
            tag,
            children: Vec::new(),
        }
    }

    pub fn with_text(tag: Tag, text: impl Into<String>) -> Self {
        Self {
            tag,
            children: vec![Node::Text(text.into())],
        }
    }

    pub fn with_opaque(tag: Tag, data: impl Into<Vec<u8>>) -> Self {
        Self {
            tag,
            children: vec![Node::Opaque(data.into())],
        }
    }

    /// Builder-style [`Element::push`].
    pub fn child(mut self, child: Element) -> Self {
        self.push(child);
        self
    }

    pub fn push(&mut self, child: Element) {
        self.children.push(Node::Element(child));
    }

    pub fn elements(&self) -> impl Iterator<Item = &Element> {
        self.children.iter().filter_map(|node| match node {
            Node::Element(el) => Some(el),
            _ => None,
        })
    }

    pub fn find(&self, tag: Tag) -> Option<&Element> {
        self.elements().find(|el| el.tag == tag)
    }

    pub fn find_all(&self, tag: Tag) -> impl Iterator<Item = &Element> {
        self.elements().filter(move |el| el.tag == tag)
    }

    /// The text of a text-only element. An element without children reads as "".
    pub fn text(&self) -> Option<&str> {
        match self.children.as_slice() {
            [] => Some(""),
            [Node::Text(text)] => Some(text),
            _ => None,
        }
    }

    pub fn child_text(&self, tag: Tag) -> Option<&str> {
        self.find(tag)?.text()
    }

    pub fn opaque(&self) -> Option<&[u8]> {
        match self.children.as_slice() {
            [Node::Opaque(data)] => Some(data),
            _ => None,
        }
    }
}
