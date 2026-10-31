//! WBXML bytes to an [`Element`] tree. String tables, entities and opaque
//! data are read; attributes, which EAS never uses, are refused.

use std::fmt;

use super::{Element, Node, Tag, encode::token};

/// Guards against stack exhaustion from hostile input; real EAS documents
/// stay well under 20 levels.
const MAX_DEPTH: usize = 64;
/// Elements in one document: thousands of Sync commands are plausible,
/// millions are not.
pub const MAX_ELEMENTS: usize = 100_000;
/// Text a document may decode to, beyond its own size: string table
/// references copy their string, so a small document could otherwise
/// expand without bound.
const TEXT_ALLOWANCE: usize = 1024 * 1024;
const UTF8: u32 = 0x6A;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodeError {
    pub offset: usize,
    pub message: String,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "WBXML decode error at byte {}: {}",
            self.offset, self.message
        )
    }
}

impl std::error::Error for DecodeError {}

pub fn decode(input: &[u8]) -> Result<Element, DecodeError> {
    let mut d = Decoder {
        input,
        pos: 0,
        page: 0,
        string_table: &[],
        elements: 0,
        text_budget: input.len().saturating_mul(2) + TEXT_ALLOWANCE,
    };
    let version = d.u8()?;
    if !(0x01..=0x03).contains(&version) {
        return Err(d.error_at(0, format!("unsupported WBXML version 0x{version:02X}")));
    }
    if d.mb_u32()? == 0 {
        // Public identifier given as a string table offset.
        d.mb_u32()?;
    }
    let charset = d.mb_u32()?;
    if charset != UTF8 {
        return Err(d.error(format!("unsupported charset MIBenum {charset}")));
    }
    let table_len = d.mb_u32()? as usize;
    d.string_table = d.take(table_len)?;

    while d.peek()? == token::SWITCH_PAGE {
        d.pos += 1;
        d.page = d.u8()?;
    }
    let root = d.element(0)?;
    if d.pos != input.len() {
        return Err(d.error(format!("{} trailing bytes", input.len() - d.pos)));
    }
    Ok(root)
}

struct Decoder<'a> {
    input: &'a [u8],
    pos: usize,
    /// The active code page; WBXML keeps it across element boundaries.
    page: u8,
    string_table: &'a [u8],
    elements: usize,
    /// Bytes of text and blobs still allowed (see [`TEXT_ALLOWANCE`]).
    text_budget: usize,
}

impl<'a> Decoder<'a> {
    /// Accounts for `bytes` of decoded text.
    fn spend(&mut self, at: usize, bytes: usize) -> Result<(), DecodeError> {
        self.text_budget = self
            .text_budget
            .checked_sub(bytes)
            .ok_or_else(|| self.error_at(at, "document expands to too much text"))?;
        Ok(())
    }

    fn element(&mut self, depth: usize) -> Result<Element, DecodeError> {
        if depth > MAX_DEPTH {
            return Err(self.error("document nested too deeply"));
        }
        self.elements += 1;
        if self.elements > MAX_ELEMENTS {
            return Err(self.error(format!("more than {MAX_ELEMENTS} elements")));
        }
        let start = self.pos;
        let byte = self.u8()?;
        if byte & token::ATTRIBUTES != 0 {
            return Err(self.error_at(start, "attributes are not supported"));
        }
        let mut element = Element::new(Tag {
            page: self.page,
            token: byte & token::TAG_MASK,
        });
        if byte & token::CONTENT == 0 {
            return Ok(element);
        }
        loop {
            let at = self.pos;
            match self.u8()? {
                token::END => return Ok(element),
                token::SWITCH_PAGE => self.page = self.u8()?,
                token::STR_I => {
                    let text = self.inline_str()?;
                    self.spend(at, text.len())?;
                    push_text(&mut element, text);
                }
                token::STR_T => {
                    let offset = self.mb_u32()? as usize;
                    let text = self.table_str(at, offset)?;
                    self.spend(at, text.len())?;
                    push_text(&mut element, text);
                }
                token::ENTITY => {
                    let code = self.mb_u32()?;
                    let c = char::from_u32(code)
                        .ok_or_else(|| self.error_at(at, format!("invalid entity {code}")))?;
                    self.spend(at, c.len_utf8())?;
                    push_text(&mut element, c.to_string());
                }
                token::OPAQUE => {
                    let len = self.mb_u32()? as usize;
                    let data = self.take(len)?.to_vec();
                    self.spend(at, data.len())?;
                    element.children.push(Node::Opaque(data));
                }
                b if b & token::TAG_MASK >= token::FIRST_TAG && b & token::ATTRIBUTES == 0 => {
                    self.pos = at;
                    let child = self.element(depth + 1)?;
                    element.push(child);
                }
                b => return Err(self.error_at(at, format!("unsupported token 0x{b:02X}"))),
            }
        }
    }

    fn inline_str(&mut self) -> Result<String, DecodeError> {
        let start = self.pos;
        let len = self.input[start..]
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| self.error("unterminated inline string"))?;
        self.pos = start + len + 1;
        String::from_utf8(self.input[start..start + len].to_vec())
            .map_err(|_| self.error_at(start, "inline string is not UTF-8"))
    }

    fn table_str(&self, at: usize, offset: usize) -> Result<String, DecodeError> {
        let tail = self
            .string_table
            .get(offset..)
            .ok_or_else(|| self.error_at(at, "string table offset out of range"))?;
        let len = tail
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| self.error_at(at, "unterminated string table entry"))?;
        String::from_utf8(tail[..len].to_vec())
            .map_err(|_| self.error_at(at, "string table entry is not UTF-8"))
    }

    fn peek(&self) -> Result<u8, DecodeError> {
        self.input
            .get(self.pos)
            .copied()
            .ok_or_else(|| self.error("unexpected end of input"))
    }

    fn u8(&mut self) -> Result<u8, DecodeError> {
        let b = self.peek()?;
        self.pos += 1;
        Ok(b)
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], DecodeError> {
        let end = self
            .pos
            .checked_add(len)
            .filter(|&end| end <= self.input.len())
            .ok_or_else(|| self.error(format!("need {len} bytes, input too short")))?;
        let bytes = &self.input[self.pos..end];
        self.pos = end;
        Ok(bytes)
    }

    /// Multi-byte unsigned integer: 7 bits per byte, high bit means "more".
    fn mb_u32(&mut self) -> Result<u32, DecodeError> {
        let start = self.pos;
        let mut value: u32 = 0;
        for _ in 0..5 {
            let b = self.u8()?;
            value = value
                .checked_mul(128)
                .ok_or_else(|| self.error_at(start, "multi-byte integer overflow"))?
                | u32::from(b & 0x7F);
            if b & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(self.error_at(start, "multi-byte integer too long"))
    }

    fn error(&self, message: impl Into<String>) -> DecodeError {
        self.error_at(self.pos, message)
    }

    fn error_at(&self, offset: usize, message: impl Into<String>) -> DecodeError {
        DecodeError {
            offset,
            message: message.into(),
        }
    }
}

/// Adjacent text pieces (e.g. STR_I, ENTITY, STR_I) become one text node.
fn push_text(element: &mut Element, text: String) {
    match element.children.last_mut() {
        Some(Node::Text(existing)) => existing.push_str(&text),
        _ => element.children.push(Node::Text(text)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wbxml::cp::{airsync, folder_hierarchy};

    const HEADER: [u8; 4] = [0x03, 0x01, 0x6A, 0x00];

    #[test]
    fn refuses_documents_that_expand() {
        // A 64 KiB string table entry referenced 100,000 times.
        let entry = vec![b'x'; 64 * 1024];
        let mut doc = vec![0x03, 0x01, 0x6A];
        let len = entry.len() + 1;
        doc.extend([
            0x80 | (len >> 14) as u8,
            0x80 | ((len >> 7) & 0x7F) as u8,
            (len & 0x7F) as u8,
        ]);
        doc.extend(&entry);
        doc.push(0);
        doc.push(0x45); // Sync with content
        for _ in 0..100_000 {
            doc.extend([0x83, 0x00]); // STR_T offset 0
        }
        doc.push(0x01);
        let error = decode(&doc).unwrap_err();
        assert!(error.message.contains("too much text"), "{error}");
    }

    #[test]
    fn refuses_too_many_elements() {
        let mut doc = HEADER.to_vec();
        doc.push(0x45); // Sync with content
        doc.extend(std::iter::repeat_n(0x05, MAX_ELEMENTS + 1)); // empty Syncs
        doc.push(0x01);
        let error = decode(&doc).unwrap_err();
        assert!(error.message.contains("elements"), "{error}");
    }

    fn doc(body: &[u8]) -> Vec<u8> {
        [&HEADER[..], body].concat()
    }

    #[test]
    fn decodes_page_switch_and_text() {
        let el = decode(&doc(&[
            0x00, 0x07, 0x56, 0x52, 0x03, b'0', 0x00, 0x01, 0x01,
        ]))
        .unwrap();
        assert_eq!(el.tag, folder_hierarchy::FolderSync);
        assert_eq!(el.child_text(folder_hierarchy::SyncKey), Some("0"));
    }

    #[test]
    fn code_page_persists_after_end() {
        // Token 0x0C after the closed SyncKey is still on page 7 (Status).
        let el = decode(&doc(&[
            0x45, 0x00, 0x07, 0x52, 0x03, b'1', 0x00, 0x01, 0x0C, 0x01,
        ]))
        .unwrap();
        assert_eq!(el.tag, airsync::Sync);
        let tags: Vec<Tag> = el.elements().map(|e| e.tag).collect();
        assert_eq!(
            tags,
            vec![folder_hierarchy::SyncKey, folder_hierarchy::Status]
        );
    }

    #[test]
    fn decodes_opaque_and_multibyte_length() {
        let data = vec![0xAB; 200];
        let mut body = vec![0x45, 0x0B | 0x40, 0xC3, 0x81, 0x48];
        body.extend(&data);
        body.extend([0x01, 0x01]);
        let el = decode(&doc(&body)).unwrap();
        assert_eq!(el.find(airsync::SyncKey).unwrap().opaque(), Some(&data[..]));
    }

    #[test]
    fn merges_entities_into_text() {
        let el = decode(&doc(&[
            0x4B, 0x03, b'a', 0x00, 0x02, 0x26, 0x03, b'b', 0x00, 0x01,
        ]))
        .unwrap();
        assert_eq!(el.text(), Some("a&b"));
    }

    #[test]
    fn reads_string_table() {
        let mut bytes = vec![0x03, 0x01, 0x6A, 0x04, b'x', 0x00, b'y', 0x00];
        bytes.extend([0x4B, 0x83, 0x02, 0x01]);
        assert_eq!(decode(&bytes).unwrap().text(), Some("y"));
    }

    #[test]
    fn rejects_truncated_input() {
        let err = decode(&doc(&[0x45, 0x4B, 0x03, b'1'])).unwrap_err();
        assert!(err.message.contains("unterminated"), "{err}");
        assert!(decode(&doc(&[0x45, 0x4B])).is_err());
    }

    #[test]
    fn rejects_trailing_bytes_and_attributes() {
        assert!(decode(&doc(&[0x05, 0x05])).is_err());
        assert!(decode(&doc(&[0x85, 0x01])).is_err());
    }

    #[test]
    fn rejects_deep_nesting() {
        let mut body = vec![0x45; MAX_DEPTH + 2];
        body.extend(vec![0x01; MAX_DEPTH + 2]);
        let err = decode(&doc(&body)).unwrap_err();
        assert!(err.message.contains("deeply"), "{err}");
    }
}
