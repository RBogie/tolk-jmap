//! Request line parameters (MS-ASHTTP 2.2.1.1.1), plain or base64 encoded.

use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Serialize;

#[derive(Debug, Default, Serialize)]
pub struct EasQuery {
    pub cmd: Option<String>,
    pub user: Option<String>,
    pub device_id: Option<String>,
    pub device_type: Option<String>,
    /// Remaining parameters, e.g. `CollectionId`, `ItemId`, `SaveInSent`.
    pub params: Vec<(String, String)>,
    /// Only present in the base64 form; plain requests send `X-MS-PolicyKey`.
    pub policy_key: Option<u32>,
    /// Only in the base64 form, e.g. 141 for 14.1.
    pub protocol_version: Option<u16>,
    pub base64: bool,
}

pub fn parse(query: Option<&str>) -> EasQuery {
    let Some(query) = query.filter(|q| !q.is_empty()) else {
        return EasQuery::default();
    };
    if query.contains('=') && query.contains('&') || query.contains("Cmd=") {
        return parse_plain(query);
    }
    let encoded: String = form_urlencoded::parse(format!("q={query}").as_bytes())
        .map(|(_, v)| v.into_owned())
        .next()
        .unwrap_or_default();
    STANDARD
        .decode(encoded.trim())
        .ok()
        .and_then(|bytes| parse_base64(&bytes))
        .unwrap_or_default()
}

fn parse_plain(query: &str) -> EasQuery {
    let mut out = EasQuery::default();
    for (key, value) in form_urlencoded::parse(query.as_bytes()) {
        let value = value.into_owned();
        match key.as_ref() {
            "Cmd" => out.cmd = Some(value),
            "User" => out.user = Some(value),
            "DeviceId" => out.device_id = Some(value),
            "DeviceType" => out.device_type = Some(value),
            _ => out.params.push((key.into_owned(), value)),
        }
    }
    out
}

fn parse_base64(bytes: &[u8]) -> Option<EasQuery> {
    let mut r = Reader(bytes);
    let protocol_version = r.u8()?;
    let cmd = command_name(r.u8()?)?;
    let _locale = r.take(2)?;
    let device_id = r.short_bytes()?;
    let policy_key = match r.short_bytes()? {
        [] => None,
        key => Some(u32::from_le_bytes(key.try_into().ok()?)),
    };
    let device_type = r.short_bytes()?;

    let mut out = EasQuery {
        cmd: Some(cmd.to_owned()),
        device_id: Some(String::from_utf8_lossy(device_id).into_owned()),
        device_type: Some(String::from_utf8_lossy(device_type).into_owned()),
        policy_key,
        protocol_version: Some(u16::from(protocol_version)),
        base64: true,
        ..EasQuery::default()
    };
    while !r.0.is_empty() {
        let tag = r.u8()?;
        let value = r.short_bytes()?;
        let value = String::from_utf8_lossy(value).into_owned();
        match tag {
            8 => out.user = Some(value),
            _ => out.params.push((param_name(tag).to_owned(), value)),
        }
    }
    Some(out)
}

fn command_name(code: u8) -> Option<&'static str> {
    Some(match code {
        0 => "Sync",
        1 => "SendMail",
        2 => "SmartForward",
        3 => "SmartReply",
        4 => "GetAttachment",
        9 => "FolderSync",
        10 => "FolderCreate",
        11 => "FolderDelete",
        12 => "FolderUpdate",
        13 => "MoveItems",
        14 => "GetItemEstimate",
        15 => "MeetingResponse",
        16 => "Search",
        17 => "Settings",
        18 => "Ping",
        19 => "ItemOperations",
        20 => "Provision",
        21 => "ResolveRecipients",
        22 => "ValidateCert",
        23 => "Find",
        _ => return None,
    })
}

fn param_name(tag: u8) -> &'static str {
    match tag {
        0 => "AttachmentName",
        1 => "CollectionId",
        3 => "ItemId",
        4 => "LongId",
        6 => "Occurrence",
        7 => "Options",
        _ => "Unknown",
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Some(head)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }

    /// A one-byte length followed by that many bytes.
    fn short_bytes(&mut self) -> Option<&'a [u8]> {
        let len = self.u8()? as usize;
        self.take(len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_query() {
        let q = parse(Some(
            "Cmd=Sync&User=rob%40example.com&DeviceId=androidc123&DeviceType=Android",
        ));
        assert_eq!(q.cmd.as_deref(), Some("Sync"));
        assert_eq!(q.user.as_deref(), Some("rob@example.com"));
        assert_eq!(q.device_id.as_deref(), Some("androidc123"));
        assert_eq!(q.device_type.as_deref(), Some("Android"));
        assert!(!q.base64);
    }

    #[test]
    fn parses_plain_query_with_extra_params() {
        let q = parse(Some(
            "Cmd=SendMail&User=rob&DeviceId=d&DeviceType=t&SaveInSent=T",
        ));
        assert_eq!(q.params, vec![("SaveInSent".to_owned(), "T".to_owned())]);
    }

    #[test]
    fn parses_base64_query() {
        let mut raw = vec![141, 0, 0x09, 0x04]; // 14.1, Sync, en-US
        raw.extend([2, b'd', b'1']);
        raw.extend([4, 1, 0, 0, 0]);
        raw.extend([3, b'P', b'h', b'n']);
        raw.extend([8, 3, b'r', b'o', b'b']);
        raw.extend([1, 1, b'5']);
        let encoded = STANDARD
            .encode(&raw)
            .replace('+', "%2B")
            .replace('/', "%2F");
        let q = parse(Some(&encoded));
        assert!(q.base64);
        assert_eq!(q.cmd.as_deref(), Some("Sync"));
        assert_eq!(q.device_id.as_deref(), Some("d1"));
        assert_eq!(q.device_type.as_deref(), Some("Phn"));
        assert_eq!(q.policy_key, Some(1));
        assert_eq!(q.user.as_deref(), Some("rob"));
        assert_eq!(q.params, vec![("CollectionId".to_owned(), "5".to_owned())]);
    }
}

#[cfg(test)]
mod proptests {
    use proptest::prelude::*;

    use super::*;

    proptest! {
        #[test]
        fn parsing_any_query_never_panics(query in ".{0,200}") {
            let _ = parse(Some(&query));
        }

        #[test]
        fn parsing_any_base64_query_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..200)) {
            let _ = parse(Some(&STANDARD.encode(bytes)));
        }
    }
}
