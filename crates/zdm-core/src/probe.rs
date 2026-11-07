use anyhow::{Context, Result};
use reqwest::header::{HeaderName, HeaderValue, ACCEPT_ENCODING, CONTENT_DISPOSITION, CONTENT_LENGTH,
    CONTENT_RANGE, ETAG, LAST_MODIFIED, RANGE, REFERER, USER_AGENT};
use reqwest::{Client, StatusCode};

use crate::engine::Options;

/// What the server told us about the resource.
#[derive(Debug, Clone)]
pub struct Probe {
    /// URL after redirects.
    pub final_url: String,
    /// Total size if known.
    pub size: Option<u64>,
    /// True only if the server answered a Range request with 206.
    pub ranges: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// Suggested, sanitized file name.
    pub filename: String,
}

pub fn build_client(opts: &Options) -> Result<Client> {
    let mut h = reqwest::header::HeaderMap::new();
    h.insert(USER_AGENT, HeaderValue::from_str(&opts.user_agent).context("bad user agent")?);
    // We store bytes exactly as sent so that ranges line up with the file.
    h.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    if let Some(r) = &opts.referer {
        h.insert(REFERER, HeaderValue::from_str(r).context("bad referer")?);
    }
    for (k, v) in &opts.headers {
        h.insert(
            HeaderName::from_bytes(k.as_bytes()).with_context(|| format!("bad header name {k}"))?,
            HeaderValue::from_str(v).with_context(|| format!("bad header value for {k}"))?,
        );
    }
    Ok(Client::builder().default_headers(h).build()?)
}

/// Learn size / range support with a single `Range: bytes=0-0` request.
pub async fn probe(client: &Client, url: &str) -> Result<Probe> {
    let resp = client.get(url).header(RANGE, "bytes=0-0").send().await.context("probe request failed")?;
    let status = resp.status();
    if !(status.is_success()) {
        anyhow::bail!("server returned {status} for {url}");
    }
    let hv = |name| resp.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
    let (ranges, size) = if status == StatusCode::PARTIAL_CONTENT {
        let total = hv(CONTENT_RANGE).and_then(|v| v.rsplit('/').next().and_then(|t| t.trim().parse::<u64>().ok()));
        (total.is_some(), total)
    } else {
        (false, hv(CONTENT_LENGTH).and_then(|v| v.parse().ok()))
    };
    let final_url = resp.url().to_string();
    let filename = hv(CONTENT_DISPOSITION)
        .and_then(|d| filename_from_disposition(&d))
        .or_else(|| filename_from_url(resp.url().path()))
        .unwrap_or_else(|| "download".to_string());
    Ok(Probe { final_url, size, ranges, etag: hv(ETAG), last_modified: hv(LAST_MODIFIED), filename })
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() && s.is_char_boundary(i + 1) && s.is_char_boundary(i + 3) {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn sanitize(name: &str) -> Option<String> {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_control() || "/\\:*?\"<>|".contains(c) { '_' } else { c })
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').to_string();
    if cleaned.is_empty() { None } else { Some(cleaned) }
}

fn filename_from_url(path: &str) -> Option<String> {
    sanitize(&percent_decode(path.rsplit('/').next().unwrap_or("")))
}

/// Parse `filename*=UTF-8''...` (RFC 6266 / 5987) or `filename="..."`.
pub fn filename_from_disposition(value: &str) -> Option<String> {
    let mut plain = None;
    for part in value.split(';').skip(1) {
        let (k, v) = part.split_once('=')?;
        let k = k.trim().to_ascii_lowercase();
        let v = v.trim();
        if k == "filename*" {
            if let Some((_, rest)) = v.split_once("''") {
                if let Some(n) = sanitize(&percent_decode(rest)) {
                    return Some(n);
                }
            }
        } else if k == "filename" {
            plain = sanitize(v.trim_matches('"'));
        }
    }
    plain
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disposition_quoted() {
        assert_eq!(filename_from_disposition("attachment; filename=\"report final.bin\"").unwrap(), "report final.bin");
    }

    #[test]
    fn disposition_extended_wins() {
        let v = "attachment; filename=\"x.txt\"; filename*=UTF-8''na%C3%AFve%20file.txt";
        assert_eq!(filename_from_disposition(v).unwrap(), "naïve file.txt");
    }

    #[test]
    fn disposition_blocks_traversal() {
        assert_eq!(filename_from_disposition("attachment; filename=\"../../etc/passwd\"").unwrap(), "_.._etc_passwd");
    }

    #[test]
    fn url_name() {
        assert_eq!(filename_from_url("/a/b/my%20file.zip").unwrap(), "my file.zip");
        assert!(filename_from_url("/a/").is_none());
    }
}
