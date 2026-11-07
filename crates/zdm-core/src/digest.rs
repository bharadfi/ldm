//! HTTP Digest authentication (RFC 7616 / 2617): MD5 and SHA-256 with `qop=auth` or no qop.
//! Not supported (the request then simply fails with 401): the `-sess` variants and `qop=auth-int`.
//!
//! The challenge is learned from the first `401` and then used to sign every later request; a new
//! challenge (expired nonce) is learned the same way.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use md5::{Digest as _, Md5};
use reqwest::header::{AUTHORIZATION, WWW_AUTHENTICATE};
use reqwest::{RequestBuilder, Response, StatusCode};
use sha2::Sha256;
use url::Url;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Algo {
    Md5,
    Sha256,
}

impl Algo {
    fn hash(self, s: &str) -> String {
        match self {
            Algo::Md5 => hex(&Md5::digest(s.as_bytes())),
            Algo::Sha256 => hex(&Sha256::digest(s.as_bytes())),
        }
    }
    fn name(self) -> &'static str {
        match self {
            Algo::Md5 => "MD5",
            Algo::Sha256 => "SHA-256",
        }
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// `key=value, key2="quoted, value"` -> pairs with lower-cased keys and unquoted values.
pub(crate) fn parse_params(s: &str) -> Vec<(String, String)> {
    let (mut out, mut key, mut val, mut in_key, mut quoted) = (vec![], String::new(), String::new(), true, false);
    for c in s.chars().chain(std::iter::once(',')) {
        match c {
            '=' if in_key => in_key = false,
            '"' => quoted = !quoted,
            ',' if !quoted => {
                if !key.trim().is_empty() {
                    out.push((key.trim().to_ascii_lowercase(), val.trim().to_string()));
                }
                key.clear();
                val.clear();
                in_key = true;
            }
            c => {
                if in_key { key.push(c) } else { val.push(c) }
            }
        }
    }
    out
}

fn param<'a>(p: &'a [(String, String)], k: &str) -> Option<&'a str> {
    p.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
}

#[derive(Debug)]
pub struct Digest {
    user: String,
    pass: String,
    realm: String,
    nonce: String,
    opaque: Option<String>,
    qop_auth: bool,
    algo: Algo,
    nc: AtomicU32,
}

/// The response value (also used by the test server to check clients).
#[allow(clippy::too_many_arguments)] // the parameters of the RFC formula
pub(crate) fn compute_response(algo_name: &str, user: &str, pass: &str, realm: &str, method: &str, uri: &str, nonce: &str, qop_auth: bool, nc: &str, cnonce: &str) -> String {
    let algo = if algo_name.eq_ignore_ascii_case("SHA-256") { Algo::Sha256 } else { Algo::Md5 };
    let ha1 = algo.hash(&format!("{user}:{realm}:{pass}"));
    let ha2 = algo.hash(&format!("{method}:{uri}"));
    if qop_auth {
        algo.hash(&format!("{ha1}:{nonce}:{nc}:{cnonce}:auth:{ha2}"))
    } else {
        algo.hash(&format!("{ha1}:{nonce}:{ha2}"))
    }
}

impl Digest {
    /// `None` when the challenge is not Digest or uses something unsupported.
    pub fn from_challenge(header: &str, user: &str, pass: &str) -> Option<Digest> {
        let rest = header.trim().strip_prefix("Digest ").or_else(|| header.trim().strip_prefix("digest "))?;
        let p = parse_params(rest);
        let algo = match param(&p, "algorithm").map(str::to_ascii_uppercase).as_deref() {
            None | Some("MD5") => Algo::Md5,
            Some("SHA-256") => Algo::Sha256,
            _ => return None,
        };
        let qops: Vec<&str> = param(&p, "qop").map(|q| q.split(',').map(str::trim).collect()).unwrap_or_default();
        let qop_auth = qops.contains(&"auth");
        if !qops.is_empty() && !qop_auth {
            return None; // only auth-int offered
        }
        Some(Digest {
            user: user.to_string(),
            pass: pass.to_string(),
            realm: param(&p, "realm")?.to_string(),
            nonce: param(&p, "nonce")?.to_string(),
            opaque: param(&p, "opaque").map(str::to_string),
            qop_auth,
            algo,
            nc: AtomicU32::new(0),
        })
    }

    fn header_with(&self, method: &str, uri: &str, cnonce: &str) -> String {
        let nc = format!("{:08x}", self.nc.fetch_add(1, Ordering::SeqCst) + 1);
        let response = compute_response(self.algo.name(), &self.user, &self.pass, &self.realm, method, uri, &self.nonce, self.qop_auth, &nc, cnonce);
        let mut h = format!("Digest username=\"{}\", realm=\"{}\", nonce=\"{}\", uri=\"{uri}\", algorithm={}, response=\"{response}\"", self.user, self.realm, self.nonce, self.algo.name());
        if let Some(o) = &self.opaque {
            h += &format!(", opaque=\"{o}\"");
        }
        if self.qop_auth {
            h += &format!(", qop=auth, nc={nc}, cnonce=\"{cnonce}\"");
        }
        h
    }

    /// `Authorization` header value for one request.
    pub fn header(&self, method: &str, uri: &str) -> String {
        let cnonce = hex(&rand::random::<[u8; 8]>());
        self.header_with(method, uri, &cnonce)
    }
}

/// User name and password plus whatever Digest challenge has been learned. Shared by all connections of
/// a download. Without a user name it does nothing.
#[derive(Default)]
pub struct Credentials {
    user: Option<(String, String)>,
    digest: Mutex<Option<Arc<Digest>>>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Credentials({})", if self.user.is_some() { "set" } else { "none" })
    }
}

impl Credentials {
    pub fn new(user: &str, pass: &str) -> Credentials {
        Credentials { user: Some((user.to_string(), pass.to_string())), digest: Mutex::new(None) }
    }

    /// Add a Digest `Authorization` header once a challenge is known (otherwise the request is unchanged).
    pub fn sign(&self, req: RequestBuilder, url: &str) -> RequestBuilder {
        let d = self.digest.lock().unwrap().clone();
        match (d, Url::parse(url)) {
            (Some(d), Ok(u)) => {
                let uri = match u.query() {
                    Some(q) => format!("{}?{q}", u.path()),
                    None => u.path().to_string(),
                };
                req.header(AUTHORIZATION, d.header("GET", &uri))
            }
            _ => req,
        }
    }

    /// Call with a `401` response: remembers a Digest challenge. True when the request is worth repeating
    /// (a challenge was learned that differs from the one that just failed).
    pub fn learn(&self, resp: &Response) -> bool {
        let Some((user, pass)) = &self.user else { return false };
        if resp.status() != StatusCode::UNAUTHORIZED {
            return false;
        }
        for v in resp.headers().get_all(WWW_AUTHENTICATE) {
            let Some(d) = v.to_str().ok().and_then(|h| Digest::from_challenge(h, user, pass)) else { continue };
            let mut slot = self.digest.lock().unwrap();
            // the same nonce failing again means the password is wrong: do not loop
            if slot.as_ref().is_some_and(|old| old.nonce == d.nonce && !has_stale(v.to_str().unwrap_or(""))) {
                return false;
            }
            *slot = Some(Arc::new(d));
            return true;
        }
        false
    }
}

fn has_stale(h: &str) -> bool {
    parse_params(h.trim_start_matches("Digest ").trim_start_matches("digest ")).iter().any(|(k, v)| k == "stale" && v.eq_ignore_ascii_case("true"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The worked example of RFC 2617 section 3.5.
    #[test]
    fn rfc_example_response() {
        let r = compute_response("MD5", "Mufasa", "Circle Of Life", "testrealm@host.com", "GET", "/dir/index.html", "dcd98b7102dd2f0e8b11d0f600bfb0c093", true, "00000001", "0a4f113b");
        assert_eq!(r, "6629fae49393a05397450978507c4ef1");
    }

    /// RFC 7616 section 3.9.1 (SHA-256).
    #[test]
    fn rfc7616_sha256_example() {
        let r = compute_response("SHA-256", "Mufasa", "Circle of Life", "http-auth@example.org", "GET", "/dir/index.html", "7ypf/xlj9XXwfDPEoM4URrv/xwf94BcCAzFZH4GiTo0v", true, "00000001", "f2/wE4q74E6zIJEtWaHKaf5wv/H5QzzpXusqGemxURZJ");
        assert_eq!(r, "753927fa0e85d155564e2e272a28d1802ca10daf4496794697cf8db5856cb6c1");
    }

    #[test]
    fn challenge_parsing_and_header() {
        let c = r#"Digest realm="r, with comma", nonce="abc", qop="auth,auth-int", algorithm=MD5, opaque="op""#;
        let d = Digest::from_challenge(c, "u", "p").unwrap();
        let h = d.header_with("GET", "/f?x=1", "cn");
        assert!(h.contains(r#"realm="r, with comma""#) && h.contains("nc=00000001") && h.contains(r#"cnonce="cn""#) && h.contains(r#"opaque="op""#) && h.contains("qop=auth"), "{h}");
        assert!(d.header_with("GET", "/f", "cn").contains("nc=00000002"), "the nonce count grows");
        assert!(Digest::from_challenge(r#"Basic realm="x""#, "u", "p").is_none());
        assert!(Digest::from_challenge(r#"Digest realm="x", nonce="n", qop="auth-int""#, "u", "p").is_none());
        assert!(Digest::from_challenge(r#"Digest realm="x", nonce="n", algorithm=MD5-sess, qop="auth""#, "u", "p").is_none());
        assert!(!Digest::from_challenge(r#"Digest realm="x", nonce="n""#, "u", "p").unwrap().header_with("GET", "/", "c").contains("qop"), "no qop offered, none sent");
    }
}
