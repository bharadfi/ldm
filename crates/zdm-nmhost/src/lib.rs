//! Native-messaging host: lets the browser extension hand downloads to `zdmd`.
//!
//! Wire format (browser side): 4-byte native-endian length, then UTF-8 JSON. The host accepts only
//! `ping` and `add`; it never exposes pause/remove or file deletion to web-reachable code.

pub mod install;

use std::io::{self, Read, Write};

use serde_json::{json, Value};
use zdm_proto::AddRequest;

pub const HOST_NAME: &str = "com.zdm.host";
/// Stable Chrome/Chromium/Edge/Brave extension ID (from the public `key` in the extension manifest).
pub const CHROME_EXTENSION_ID: &str = "oihpkfhbmggccackdffcmhlbficmdigk";
pub const FIREFOX_EXTENSION_ID: &str = "zdm@zdm.invalid";

/// Chrome refuses host->browser messages above 1 MiB; we also refuse the reverse to bound memory.
pub const MAX_FRAME: usize = 1 << 20;

/// Read one frame. `Ok(None)` on clean EOF before a new frame starts.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let n = u32::from_ne_bytes(len) as usize;
    if n > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("message of {n} bytes exceeds the {MAX_FRAME} byte limit")));
    }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf)?;
    Ok(Some(buf))
}

pub fn write_frame(w: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    w.write_all(&(payload.len() as u32).to_ne_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

/// A validated message from the extension.
#[derive(Debug, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum Msg {
    Ping,
    Add(AddRequest),
}

const MAX_URL: usize = 8192;
const MAX_COOKIE: usize = 16 * 1024;
/// Request headers the extension may forward. Everything else is dropped by the host.
const ALLOWED_HEADERS: &[&str] = &["cookie"];

fn opt_str(v: &Value, key: &str, max: usize) -> Result<Option<String>, String> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.len() <= max => Ok(Some(s.clone()).filter(|s| !s.is_empty())),
        Some(Value::String(_)) => Err(format!("{key} is too long")),
        Some(_) => Err(format!("{key} must be a string")),
    }
}

pub fn parse_message(v: &Value) -> Result<Msg, String> {
    match v.get("type").and_then(Value::as_str) {
        Some("ping") => Ok(Msg::Ping),
        Some("add") => {
            let url = opt_str(v, "url", MAX_URL)?.ok_or("url is required")?;
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return Err("only http:// and https:// URLs are supported".into());
            }
            let mut headers = Vec::new();
            if let Some(hs) = v.get("headers") {
                let arr = hs.as_array().ok_or("headers must be an array")?;
                if arr.len() > 8 {
                    return Err("too many headers".into());
                }
                for h in arr {
                    let pair = h.as_array().filter(|p| p.len() == 2).ok_or("each header must be [name, value]")?;
                    let (k, val) = (pair[0].as_str().ok_or("header name must be a string")?, pair[1].as_str().ok_or("header value must be a string")?);
                    if !ALLOWED_HEADERS.contains(&k.to_ascii_lowercase().as_str()) {
                        continue; // silently dropped: the extension has no business setting these
                    }
                    if val.len() > MAX_COOKIE || val.contains(['\r', '\n']) {
                        return Err(format!("invalid {k} header"));
                    }
                    headers.push((k.to_string(), val.to_string()));
                }
            }
            Ok(Msg::Add(AddRequest {
                url,
                referer: opt_str(v, "referer", MAX_URL)?.filter(|r| r.starts_with("http://") || r.starts_with("https://")),
                user_agent: opt_str(v, "user_agent", 1024)?.filter(|u| !u.contains(['\r', '\n'])),
                headers,
                start: true,
                ..Default::default()
            }))
        }
        Some(other) => Err(format!("unsupported message type: {other}")),
        None => Err("missing message type".into()),
    }
}

pub fn ok(extra: Value) -> Value {
    let mut o = json!({ "ok": true });
    if let (Some(m), Some(e)) = (o.as_object_mut(), extra.as_object()) {
        m.extend(e.clone());
    }
    o
}

pub fn err(message: impl Into<String>) -> Value {
    json!({ "ok": false, "error": message.into() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_limit() {
        let mut buf = Vec::new();
        write_frame(&mut buf, b"{\"a\":1}").unwrap();
        let mut r = io::Cursor::new(buf);
        assert_eq!(read_frame(&mut r).unwrap().unwrap(), b"{\"a\":1}");
        assert!(read_frame(&mut r).unwrap().is_none());
        let huge = ((MAX_FRAME + 1) as u32).to_ne_bytes().to_vec();
        assert!(read_frame(&mut io::Cursor::new(huge)).is_err());
        // Truncated body is an error, not a silent EOF.
        let mut trunc = 10u32.to_ne_bytes().to_vec();
        trunc.extend_from_slice(b"abc");
        assert!(read_frame(&mut io::Cursor::new(trunc)).is_err());
    }

    #[test]
    fn add_is_validated_and_filtered() {
        let m = parse_message(&json!({
            "type": "add", "url": "https://x.test/f.zip", "referer": "https://x.test/", "user_agent": "UA/1",
            "headers": [["Cookie", "a=b"], ["X-Evil", "1"], ["Authorization", "Bearer z"]]
        }))
        .unwrap();
        let Msg::Add(a) = m else { panic!() };
        assert_eq!(a.headers, vec![("Cookie".to_string(), "a=b".to_string())]);
        assert_eq!((a.referer.as_deref(), a.user_agent.as_deref(), a.start), (Some("https://x.test/"), Some("UA/1"), true));
    }

    #[test]
    fn rejects_bad_input() {
        for bad in [
            json!({"type":"add"}),
            json!({"type":"add","url":"file:///etc/passwd"}),
            json!({"type":"add","url":"ftp://h/f"}),
            json!({"type":"add","url":"http://h/","headers":[["Cookie","a\r\nX: y"]]}),
            json!({"type":"add","url":"http://h/","headers":"nope"}),
            json!({"type":"remove","id":1}),
            json!({"type":"add","url":5}),
            json!({}),
        ] {
            assert!(parse_message(&bad).is_err(), "accepted {bad}");
        }
        assert_eq!(parse_message(&json!({"type":"ping"})), Ok(Msg::Ping));
    }

    #[test]
    fn non_http_referer_is_dropped() {
        let Msg::Add(a) = parse_message(&json!({"type":"add","url":"http://h/","referer":"javascript:alert(1)"})).unwrap() else { panic!() };
        assert!(a.referer.is_none());
    }
}
