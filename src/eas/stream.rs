//! Attachments streamed into the response instead of held in memory.
//!
//! A command registers a JMAP download (status already checked) and puts
//! the returned [`Node::Stream`] where the base64 text goes. The response
//! body is then the encoded document with each download base64-encoded in
//! place as it arrives: inline WBXML strings end with a NUL, so their length
//! need not be known in advance. A download that fails halfway aborts the
//! response; the device sees a broken connection and retries.

use std::{collections::VecDeque, sync::Mutex};

use axum::body::Bytes;
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::{Stream, StreamExt, stream::BoxStream};

use crate::{
    LockExt,
    wbxml::{Chunk, Node},
};

/// The downloads a response streams, in [`Node::Stream`] order.
#[derive(Default)]
pub struct Downloads(Mutex<Vec<reqwest::Response>>);

impl Downloads {
    /// Registers a download; the node goes where its base64 text belongs.
    pub fn add(&self, download: reqwest::Response) -> Node {
        let mut downloads = self.0.locked();
        downloads.push(download);
        Node::Stream(downloads.len() - 1)
    }

    pub fn take(&self) -> Vec<reqwest::Response> {
        std::mem::take(&mut *self.0.locked())
    }
}

/// The body's length, when every download says how long it is.
pub fn length(chunks: &[Chunk], downloads: &[reqwest::Response]) -> Option<u64> {
    chunks.iter().try_fold(0, |total, chunk| {
        Some(
            total
                + match chunk {
                    Chunk::Bytes(bytes) => bytes.len() as u64,
                    Chunk::Stream(n) => downloads.get(*n)?.content_length()?.div_ceil(3) * 4,
                },
        )
    })
}

struct State {
    chunks: VecDeque<Chunk>,
    downloads: Vec<Option<reqwest::Response>>,
    /// The download being sent and the bytes not yet encoded (fewer than 3).
    current: Option<(BoxStream<'static, reqwest::Result<Bytes>>, Vec<u8>)>,
    failed: bool,
}

/// The response body: the chunks with each download base64-encoded.
pub fn body(
    chunks: Vec<Chunk>,
    downloads: Vec<reqwest::Response>,
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
            if let Some((download, carry)) = &mut state.current {
                match download.next().await {
                    Some(Ok(bytes)) => {
                        carry.extend_from_slice(&bytes);
                        let whole = carry.len() / 3 * 3;
                        if whole == 0 {
                            continue;
                        }
                        let encoded = STANDARD.encode(&carry[..whole]);
                        carry.drain(..whole);
                        return Some((Ok(Bytes::from(encoded)), state));
                    }
                    Some(Err(e)) => {
                        tracing::warn!(error = format!("{e:#}"), "streamed download failed");
                        state.failed = true;
                        return Some((Err(std::io::Error::other(e)), state));
                    }
                    None => {
                        let encoded = STANDARD.encode(&*carry);
                        state.current = None;
                        if !encoded.is_empty() {
                            return Some((Ok(Bytes::from(encoded)), state));
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
                    state.current = Some((download.bytes_stream().boxed(), Vec::new()));
                }
            }
        }
    })
}
