//! An [`Element`] tree to WBXML bytes, optionally split around streamed
//! text ([`encode_parts`]).

use super::{Element, Node};

pub(super) mod token {
    pub const SWITCH_PAGE: u8 = 0x00;
    pub const END: u8 = 0x01;
    pub const ENTITY: u8 = 0x02;
    pub const STR_I: u8 = 0x03;
    pub const STR_T: u8 = 0x83;
    pub const OPAQUE: u8 = 0xC3;

    pub const TAG_MASK: u8 = 0x3F;
    pub const CONTENT: u8 = 0x40;
    pub const ATTRIBUTES: u8 = 0x80;
    /// Tokens 0x00-0x04 are global tokens, so tag tokens start at 0x05.
    pub const FIRST_TAG: u8 = 0x05;
}

/// Version 1.3, unknown public id, UTF-8, empty string table.
const HEADER: [u8; 4] = [0x03, 0x01, 0x6A, 0x00];

pub fn encode(root: &Element) -> Vec<u8> {
    let mut out = Vec::new();
    for chunk in encode_parts(root) {
        match chunk {
            Chunk::Bytes(bytes) => out.extend(bytes),
            Chunk::Stream(_) => out.extend(STREAM_PLACEHOLDER),
        }
    }
    out
}

/// What [`encode`] writes for a [`Node::Stream`].
const STREAM_PLACEHOLDER: &[u8] = b"[streamed]";

/// A piece of an encoded document: bytes, or the place where the text of a
/// [`Node::Stream`] goes (inline, so it must not contain NUL; its STR_I and
/// terminator are in the surrounding bytes).
#[derive(Debug, PartialEq, Eq)]
pub enum Chunk {
    Bytes(Vec<u8>),
    Stream(usize),
}

/// The document as chunks, split where streamed text goes.
pub fn encode_parts(root: &Element) -> Vec<Chunk> {
    let mut encoder = Encoder {
        out: HEADER.to_vec(),
        page: 0,
        chunks: Vec::new(),
    };
    encoder.element(root);
    let mut chunks = encoder.chunks;
    chunks.push(Chunk::Bytes(encoder.out));
    chunks
}

struct Encoder {
    out: Vec<u8>,
    page: u8,
    chunks: Vec<Chunk>,
}

impl Encoder {
    fn element(&mut self, element: &Element) {
        if element.tag.page != self.page {
            self.out.extend([token::SWITCH_PAGE, element.tag.page]);
            self.page = element.tag.page;
        }
        if element.children.is_empty() {
            self.out.push(element.tag.token);
            return;
        }
        self.out.push(element.tag.token | token::CONTENT);
        for child in &element.children {
            match child {
                Node::Element(el) => self.element(el),
                // STR_I is NUL terminated. Sending such text as OPAQUE makes
                // Gmail reject the whole response (it expects a string), so
                // the NULs are dropped; mail bodies sometimes carry one.
                Node::Text(text) if text.contains('\0') => {
                    self.out.push(token::STR_I);
                    self.out.extend(text.bytes().filter(|b| *b != 0));
                    self.out.push(0);
                }
                Node::Text(text) => {
                    self.out.push(token::STR_I);
                    self.out.extend(text.as_bytes());
                    self.out.push(0);
                }
                Node::Opaque(data) => self.opaque(data),
                Node::Stream(n) => {
                    self.out.push(token::STR_I);
                    self.chunks
                        .push(Chunk::Bytes(std::mem::take(&mut self.out)));
                    self.chunks.push(Chunk::Stream(*n));
                    self.out.push(0);
                }
            }
        }
        self.out.push(token::END);
    }

    fn opaque(&mut self, data: &[u8]) {
        self.out.push(token::OPAQUE);
        push_mb_u32(&mut self.out, data.len() as u32);
        self.out.extend(data);
    }
}

fn push_mb_u32(out: &mut Vec<u8>, mut value: u32) {
    let mut buf = [0u8; 5];
    let mut i = buf.len() - 1;
    buf[i] = (value & 0x7F) as u8;
    value >>= 7;
    while value > 0 {
        i -= 1;
        buf[i] = (value & 0x7F) as u8 | 0x80;
        value >>= 7;
    }
    out.extend(&buf[i..]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wbxml::{
        cp::{airsync, airsyncbase, folder_hierarchy},
        decode,
    };

    #[test]
    fn splits_around_streamed_text() {
        let root = Element::new(airsync::Sync).child(Element {
            tag: airsync::Status,
            children: vec![Node::Stream(0)],
        });
        let chunks = encode_parts(&root);
        let [Chunk::Bytes(head), Chunk::Stream(0), Chunk::Bytes(tail)] = chunks.as_slice() else {
            panic!("{chunks:?}");
        };
        let mut whole = head.clone();
        whole.extend(b"42");
        whole.extend(tail);
        let decoded = decode(&whole).unwrap();
        assert_eq!(decoded.child_text(airsync::Status), Some("42"));
    }

    #[test]
    fn multibyte_integers() {
        for (value, expected) in [
            (0u32, vec![0x00]),
            (0x7F, vec![0x7F]),
            (0x80, vec![0x81, 0x00]),
            (0xA0, vec![0x81, 0x20]),
            (u32::MAX, vec![0x8F, 0xFF, 0xFF, 0xFF, 0x7F]),
        ] {
            let mut out = Vec::new();
            push_mb_u32(&mut out, value);
            assert_eq!(out, expected, "{value}");
        }
    }

    #[test]
    fn encodes_page_switches_only_when_needed() {
        let el = Element::new(folder_hierarchy::FolderSync)
            .child(Element::with_text(folder_hierarchy::Status, "1"))
            .child(Element::with_text(folder_hierarchy::SyncKey, "1"));
        assert_eq!(
            encode(&el),
            [
                &HEADER[..],
                &[0x00, 0x07, 0x56, 0x4C, 0x03, b'1', 0x00, 0x01],
                &[0x52, 0x03, b'1', 0x00, 0x01, 0x01],
            ]
            .concat()
        );
    }

    #[test]
    fn round_trips_mixed_pages_and_opaque() {
        let el = Element::new(airsync::Sync).child(
            Element::new(airsync::Collections).child(
                Element::new(airsync::Collection)
                    .child(Element::with_text(airsync::SyncKey, "42"))
                    .child(
                        Element::new(airsyncbase::Body)
                            .child(Element::with_text(airsyncbase::Type, "4"))
                            .child(Element::with_opaque(airsyncbase::Data, vec![0, 1, 2, 255])),
                    )
                    .child(Element::new(airsync::GetChanges))
                    .child(Element::with_text(airsync::WindowSize, "té\0st")),
            ),
        );
        let decoded = decode(&encode(&el)).unwrap();
        let window = decoded
            .find(airsync::Collections)
            .and_then(|c| c.find(airsync::Collection))
            .and_then(|c| c.find(airsync::WindowSize))
            .unwrap();
        // Text loses its NULs; everything else is identical.
        assert_eq!(window.text(), Some("tést"));
        let mut expected = el.clone();
        let Node::Element(collections) = &mut expected.children[0] else {
            unreachable!()
        };
        let Node::Element(collection) = &mut collections.children[0] else {
            unreachable!()
        };
        collection.children[3] = Node::Element(Element::with_text(airsync::WindowSize, "tést"));
        assert_eq!(decoded, expected);
    }
}
