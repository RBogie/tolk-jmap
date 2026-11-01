//! Attachments and whole messages streamed into the response instead of
//! held in memory.
//!
//! A command registers a JMAP download (status already checked) and puts
//! the returned [`Node::Stream`] where its text goes. The response body is
//! then the encoded document with each download written in place as it
//! arrives, base64-encoded (attachments) or as text (a message's MIME):
//! inline WBXML strings end with a NUL, so their length need not be known in
//! advance. A download that fails halfway aborts the response; the device
//! sees a broken connection and retries.

use std::{collections::VecDeque, sync::Mutex};

use axum::body::Bytes;
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::{Stream, StreamExt, stream::BoxStream};

use crate::{
    LockExt,
    wbxml::{Chunk, Node},
};

/// How a download is written into the document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Base64,
    /// As UTF-8 text: invalid sequences become U+FFFD and NULs are dropped,
    /// as for any other WBXML string.
    Text,
}

pub struct Download {
    pub response: reqwest::Response,
    pub encoding: Encoding,
}

/// The downloads a response streams, in [`Node::Stream`] order.
#[derive(Default)]
pub struct Downloads(Mutex<Vec<Download>>);

impl Downloads {
    /// Registers a download; the node goes where its text belongs.
    pub fn add(&self, response: reqwest::Response, encoding: Encoding) -> Node {
        let mut downloads = self.0.locked();
        downloads.push(Download { response, encoding });
        Node::Stream(downloads.len() - 1)
    }

    pub fn take(&self) -> Vec<Download> {
        std::mem::take(&mut *self.0.locked())
    }
}

/// The body's length, when it is known in advance: every download says how
/// long it is and is base64-encoded (text can change length).
pub fn length(chunks: &[Chunk], downloads: &[Download]) -> Option<u64> {
    chunks.iter().try_fold(0, |total, chunk| {
        Some(
            total
                + match chunk {
                    Chunk::Bytes(bytes) => bytes.len() as u64,
                    Chunk::Stream(n) => {
                        let download = downloads.get(*n)?;
                        if download.encoding != Encoding::Base64 {
                            return None;
                        }
                        download.response.content_length()?.div_ceil(3) * 4
                    }
                },
        )
    })
}

struct Current {
    bytes: BoxStream<'static, reqwest::Result<Bytes>>,
    encoding: Encoding,
    /// Bytes not yet written: fewer than 3 for base64, an incomplete UTF-8
    /// sequence for text.
    carry: Vec<u8>,
}

impl Current {
    /// Writes what can be written of `carry`; at the end, all of it.
    fn take(&mut self, end: bool) -> Vec<u8> {
        match self.encoding {
            Encoding::Base64 => {
                let whole = if end {
                    self.carry.len()
                } else {
                    self.carry.len() / 3 * 3
                };
                let encoded = STANDARD.encode(&self.carry[..whole]);
                self.carry.drain(..whole);
                encoded.into_bytes()
            }
            Encoding::Text => {
                let mut out = Vec::with_capacity(self.carry.len());
                let mut rest = &self.carry[..];
                loop {
                    match std::str::from_utf8(rest) {
                        Ok(text) => {
                            out.extend_from_slice(text.as_bytes());
                            rest = &[];
                            break;
                        }
                        Err(e) => {
                            let (valid, after) = rest.split_at(e.valid_up_to());
                            out.extend_from_slice(valid);
                            match e.error_len() {
                                Some(bad) => {
                                    out.extend_from_slice("\u{FFFD}".as_bytes());
                                    rest = &after[bad..];
                                }
                                // Cut off mid-sequence: the rest may follow.
                                None if !end => {
                                    rest = after;
                                    break;
                                }
                                None => {
                                    out.extend_from_slice("\u{FFFD}".as_bytes());
                                    rest = &[];
                                    break;
                                }
                            }
                        }
                    }
                }
                self.carry = rest.to_vec();
                out.retain(|b| *b != 0);
                out
            }
        }
    }
}

struct State {
    chunks: VecDeque<Chunk>,
    downloads: Vec<Option<Download>>,
    current: Option<Current>,
    failed: bool,
}

/// The response body: the chunks with each download written in place.
pub fn body(
    chunks: Vec<Chunk>,
    downloads: Vec<Download>,
) -> impl Stream<Item = std::io::Result<Bytes>> + Send + 'static {
    let state = State {
        chunks: chunks.into(),
        downloads: downloads.into_iter().map(Some).collect(),
        current: None,
        failed: false,
    };
    futures_util::stream::unfold(state, |mut state| async move {
        if state.failed {
            return None;
        }
        loop {
            if let Some(current) = &mut state.current {
                match current.bytes.next().await {
                    Some(Ok(bytes)) => {
                        current.carry.extend_from_slice(&bytes);
                        let out = current.take(false);
                        if out.is_empty() {
                            continue;
                        }
                        return Some((Ok(Bytes::from(out)), state));
                    }
                    Some(Err(e)) => {
                        tracing::warn!(error = format!("{e:#}"), "streamed download failed");
                        state.failed = true;
                        return Some((Err(std::io::Error::other(e)), state));
                    }
                    None => {
                        let out = current.take(true);
                        state.current = None;
                        if !out.is_empty() {
                            return Some((Ok(Bytes::from(out)), state));
                        }
                    }
                }
                continue;
            }
            match state.chunks.pop_front()? {
                Chunk::Bytes(bytes) => return Some((Ok(Bytes::from(bytes)), state)),
                Chunk::Stream(n) => {
                    let Some(download) = state.downloads.get_mut(n).and_then(Option::take) else {
                        state.failed = true;
                        let missing = std::io::Error::other(format!("stream {n} is missing"));
                        return Some((Err(missing), state));
                    };
                    state.current = Some(Current {
                        bytes: download.response.bytes_stream().boxed(),
                        encoding: download.encoding,
                        carry: Vec::new(),
                    });
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(chunks: &[&[u8]]) -> Vec<u8> {
        let mut current = Current {
            bytes: futures_util::stream::empty().boxed(),
            encoding: Encoding::Text,
            carry: Vec::new(),
        };
        let mut out = Vec::new();
        for chunk in chunks {
            current.carry.extend_from_slice(chunk);
            out.extend(current.take(false));
        }
        out.extend(current.take(true));
        out
    }

    #[test]
    fn text_keeps_sequences_split_across_chunks() {
        let euro = "€".as_bytes();
        assert_eq!(
            text(&[b"5 ", &euro[..1], &euro[1..], b"!"]),
            "5 €!".as_bytes()
        );
    }

    #[test]
    fn text_replaces_invalid_bytes_and_drops_nuls() {
        assert_eq!(text(&[b"a\xFFb\0c"]), "a\u{FFFD}bc".as_bytes());
        // A sequence still incomplete at the end.
        assert_eq!(text(&[b"a", &"€".as_bytes()[..2]]), "a\u{FFFD}".as_bytes());
    }
}
