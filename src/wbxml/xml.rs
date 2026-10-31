//! Human-readable XML rendering, for traffic logs and debugging only.
//! A tag gets a `Namespace:` prefix whenever its code page differs from its
//! parent's, which mirrors where the WBXML switches pages.

use std::fmt::Write;

use super::{Element, Node, Tag};

/// Opaque data up to this size is shown as hex; longer data only by length.
const MAX_HEX_BYTES: usize = 64;

pub fn to_xml(root: &Element) -> String {
    let mut out = String::new();
    write_element(&mut out, root, None, 0);
    out
}

fn write_element(out: &mut String, el: &Element, parent_page: Option<u8>, depth: usize) {
    let name = qualified_name(el.tag, parent_page);
    let indent = "  ".repeat(depth);
    match el.children.as_slice() {
        [] => writeln!(out, "{indent}<{name}/>"),
        [Node::Text(text)] => writeln!(out, "{indent}<{name}>{}</{name}>", escape(text)),
        [Node::Opaque(data)] => writeln!(out, "{indent}<{name}>{}</{name}>", opaque(data)),
        children => {
            writeln!(out, "{indent}<{name}>").unwrap();
            for child in children {
                match child {
                    Node::Element(c) => write_element(out, c, Some(el.tag.page), depth + 1),
                    Node::Text(text) => writeln!(out, "{indent}  {}", escape(text)).unwrap(),
                    Node::Opaque(data) => writeln!(out, "{indent}  {}", opaque(data)).unwrap(),
                    Node::Stream(_) => writeln!(out, "{indent}  [streamed]").unwrap(),
                }
            }
            writeln!(out, "{indent}</{name}>")
        }
    }
    .unwrap();
}

fn qualified_name(tag: Tag, parent_page: Option<u8>) -> String {
    if parent_page == Some(tag.page) {
        match tag.name() {
            Some(name) => name.to_owned(),
            None => format!("_0x{:02X}", tag.token),
        }
    } else {
        tag.to_string()
    }
}

fn opaque(data: &[u8]) -> String {
    if data.len() > MAX_HEX_BYTES {
        return format!("[opaque {} bytes]", data.len());
    }
    let hex: String = data.iter().map(|b| format!("{b:02x}")).collect();
    format!("[opaque {} bytes: {hex}]", data.len())
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wbxml::cp::{airsync, airsyncbase};

    #[test]
    fn renders_namespaces_on_page_changes() {
        let el = Element::new(airsync::Sync).child(
            Element::new(airsyncbase::BodyPreference)
                .child(Element::with_text(airsyncbase::Type, "2"))
                .child(Element::with_opaque(airsyncbase::Data, vec![0xde, 0xad])),
        );
        assert_eq!(
            to_xml(&el),
            "<AirSync:Sync>\n  <AirSyncBase:BodyPreference>\n    <Type>2</Type>\n    \
             <Data>[opaque 2 bytes: dead]</Data>\n  </AirSyncBase:BodyPreference>\n\
             </AirSync:Sync>\n"
        );
    }
}
