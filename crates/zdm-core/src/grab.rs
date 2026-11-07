//! Site grabber: find downloadable files on a page, optionally following links to other pages of the
//! same site up to a depth. HTML is scanned with a small forgiving tag reader (no full DOM needed).

use std::collections::{HashSet, VecDeque};
use std::time::Duration;

use anyhow::Result;
use reqwest::Client;
use tokio_util::sync::CancellationToken;
use url::Url;

#[derive(Clone, Debug)]
pub struct GrabOptions {
    /// 0 = only the start page; 1 = also pages it links to; ...
    pub depth: u32,
    /// Only follow pages on the same host as the start page.
    pub same_host: bool,
    /// File extensions to collect (lowercase, no dot). Empty = every link that looks like a file.
    pub extensions: Vec<String>,
    /// Stop fetching pages after this many.
    pub max_pages: usize,
    /// Stop collecting after this many files.
    pub max_links: usize,
}

impl Default for GrabOptions {
    fn default() -> Self {
        GrabOptions { depth: 0, same_host: true, extensions: vec![], max_pages: 100, max_links: 5000 }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub url: String,
    /// Decoded last path segment.
    pub name: String,
    /// Lowercase extension without the dot (may be empty).
    pub ext: String,
    /// The page it was found on.
    pub from: String,
}

/// Extensions that mean "this is a web page", not a file to download.
const PAGE_EXTS: &[&str] = &["html", "htm", "xhtml", "php", "asp", "aspx", "jsp", "cgi", "shtml"];

pub fn ext_of(u: &Url) -> String {
    let last = u.path_segments().and_then(|mut s| s.next_back()).unwrap_or("");
    match last.rsplit_once('.') {
        Some((stem, e)) if !stem.is_empty() && e.len() <= 8 && e.chars().all(|c| c.is_ascii_alphanumeric()) => e.to_ascii_lowercase(),
        _ => String::new(),
    }
}

fn is_page(u: &Url) -> bool {
    let e = ext_of(u);
    e.is_empty() || PAGE_EXTS.contains(&e.as_str())
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
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

pub fn name_of(u: &Url) -> String {
    let last = u.path_segments().and_then(|mut s| s.next_back()).unwrap_or("");
    let n = percent_decode(last);
    if n.is_empty() { u.host_str().unwrap_or("download").to_string() } else { n }
}

/// Every URL-bearing attribute of every tag, resolved against `base`. Fragments are dropped; only
/// http(s) links are returned, once each, in document order.
pub fn extract(html: &str, base: &Url) -> Vec<Url> {
    const ATTRS: &[&str] = &["href", "src", "data-src", "data-href", "poster", "data", "srcset", "data-srcset"];
    let b = html.as_bytes();
    let mut out: Vec<Url> = vec![];
    let mut seen: HashSet<String> = HashSet::new();
    let mut push = |raw: &str, out: &mut Vec<Url>| {
        let raw = raw.trim();
        if raw.is_empty() || raw.starts_with('#') {
            return;
        }
        if let Ok(mut u) = base.join(raw) {
            u.set_fragment(None);
            if matches!(u.scheme(), "http" | "https") && seen.insert(u.to_string()) {
                out.push(u);
            }
        }
    };
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        if html[i..].starts_with("<!--") {
            i = html[i..].find("-->").map_or(b.len(), |e| i + e + 3);
            continue;
        }
        // tag name
        let mut j = i + 1;
        if j < b.len() && b[j] == b'/' {
            i = j;
            continue;
        }
        let ns = j;
        while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'-') {
            j += 1;
        }
        if j == ns {
            i += 1;
            continue;
        }
        let tag = html[ns..j].to_ascii_lowercase();
        // attributes until '>'
        while j < b.len() && b[j] != b'>' {
            while j < b.len() && (b[j].is_ascii_whitespace() || b[j] == b'/') {
                j += 1;
            }
            let an = j;
            while j < b.len() && !b[j].is_ascii_whitespace() && !matches!(b[j], b'=' | b'>' | b'/') {
                j += 1;
            }
            if j == an {
                if j < b.len() && b[j] != b'>' {
                    j += 1;
                }
                continue;
            }
            let name = html[an..j].to_ascii_lowercase();
            while j < b.len() && b[j].is_ascii_whitespace() {
                j += 1;
            }
            let mut val = "";
            if j < b.len() && b[j] == b'=' {
                j += 1;
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j < b.len() && (b[j] == b'"' || b[j] == b'\'') {
                    let q = b[j];
                    let vs = j + 1;
                    j = vs;
                    while j < b.len() && b[j] != q {
                        j += 1;
                    }
                    val = &html[vs..j.min(b.len())];
                    j = (j + 1).min(b.len());
                } else {
                    let vs = j;
                    while j < b.len() && !b[j].is_ascii_whitespace() && b[j] != b'>' {
                        j += 1;
                    }
                    val = &html[vs..j];
                }
            }
            if ATTRS.contains(&name.as_str()) {
                if name.ends_with("srcset") {
                    for part in val.split(',') {
                        if let Some(first) = part.split_whitespace().next() {
                            push(first, &mut out);
                        }
                    }
                } else {
                    push(&val.replace("&amp;", "&"), &mut out);
                }
            }
        }
        // do not scan inside <script>/<style> bodies
        i = j.max(i + 1);
        if tag == "script" || tag == "style" {
            let close = format!("</{tag}");
            let lower_rest = html[i.min(b.len())..].to_ascii_lowercase();
            i = lower_rest.find(&close).map_or(b.len(), |e| i + e);
        }
    }
    out
}

fn wanted(u: &Url, opts: &GrabOptions) -> bool {
    let e = ext_of(u);
    if opts.extensions.is_empty() {
        !is_page(u)
    } else {
        opts.extensions.contains(&e)
    }
}

/// Crawl from `start`. Returns the files found (deduplicated, in discovery order).
pub async fn crawl(client: &Client, start: &str, opts: &GrabOptions, cancel: &CancellationToken) -> Result<Vec<Found>> {
    let start = Url::parse(start)?;
    anyhow::ensure!(matches!(start.scheme(), "http" | "https"), "only http(s) addresses can be grabbed");
    let mut queue: VecDeque<(Url, u32)> = VecDeque::from([(start.clone(), 0)]);
    let mut visited: HashSet<String> = HashSet::from([start.to_string()]);
    let mut files: Vec<Found> = vec![];
    let mut fetched = 0usize;
    while let Some((page, depth)) = queue.pop_front() {
        if cancel.is_cancelled() || fetched >= opts.max_pages || files.len() >= opts.max_links {
            break;
        }
        fetched += 1;
        let resp = match tokio::time::timeout(Duration::from_secs(20), client.get(page.clone()).send()).await {
            Ok(Ok(r)) if r.status().is_success() => r,
            _ if fetched == 1 => anyhow::bail!("could not open {page}"),
            _ => continue,
        };
        let is_html = resp.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.contains("html"));
        if !is_html {
            if fetched == 1 {
                anyhow::bail!("{page} is not a web page");
            }
            continue;
        }
        let Ok(bytes) = resp.bytes().await else { continue };
        let html = String::from_utf8_lossy(&bytes[..bytes.len().min(5 << 20)]).into_owned();
        for link in extract(&html, &page) {
            if wanted(&link, opts) {
                if files.len() < opts.max_links && !files.iter().any(|f| f.url == link.as_str()) {
                    files.push(Found { url: link.to_string(), name: name_of(&link), ext: ext_of(&link), from: page.to_string() });
                }
            } else if depth < opts.depth && is_page(&link) && (!opts.same_host || link.host_str() == start.host_str()) && visited.insert(link.to_string()) {
                queue.push_back((link, depth + 1));
            }
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testserver::{serve_site, Routes};

    #[test]
    fn extracts_links_from_messy_html() {
        let base = Url::parse("http://h.test/dir/page.html").unwrap();
        let html = r##"<!-- <a href="/hidden.zip"> -->
            <a href="a.zip">A</a> <A HREF='/b/c.pdf?x=1&amp;y=2#frag'>B</A>
            <img src=img.png srcset="small.jpg 1x, big.jpg 2x"> <script>var u="<a href='/js.zip'>";</script>
            <video poster="p.jpg"><source src="//cdn.test/v.mp4"></video>
            <a href="mailto:x@y.z">m</a> <a href="javascript:void(0)">j</a> <a href="#top">t</a> <a href="a.zip">dup</a>
            <a href=https://other.test/f.7z>unquoted</a>"##;
        let got: Vec<String> = extract(html, &base).iter().map(|u| u.to_string()).collect();
        assert_eq!(
            got,
            vec![
                "http://h.test/dir/a.zip",
                "http://h.test/b/c.pdf?x=1&y=2",
                "http://h.test/dir/img.png",
                "http://h.test/dir/small.jpg",
                "http://h.test/dir/big.jpg",
                "http://h.test/dir/p.jpg",
                "http://cdn.test/v.mp4",
                "https://other.test/f.7z",
            ]
        );
    }

    #[test]
    fn names_and_extensions() {
        let u = Url::parse("http://h.test/files/My%20Doc.PDF?dl=1").unwrap();
        assert_eq!(name_of(&u), "My Doc.PDF");
        assert_eq!(ext_of(&u), "pdf");
        assert_eq!(ext_of(&Url::parse("http://h.test/dir/").unwrap()), "");
        assert_eq!(ext_of(&Url::parse("http://h.test/.hidden").unwrap()), "");
    }

    fn page(links: &[&str]) -> (String, Vec<u8>) {
        let body: String = links.iter().map(|l| format!("<a href=\"{l}\">x</a>")).collect();
        ("text/html".into(), body.into_bytes())
    }

    #[tokio::test]
    async fn crawl_respects_depth_host_and_extensions() {
        let mut r = Routes::new();
        r.insert("/".into(), page(&["a.zip", "doc.pdf", "sub/p2.html", "http://elsewhere.invalid/x.html"]));
        r.insert("/sub/p2.html".into(), page(&["b.zip", "deep/p3.html"]));
        r.insert("/sub/deep/p3.html".into(), page(&["c.zip"]));
        let site = serve_site(r).await;
        let client = Client::new();
        let cancel = CancellationToken::new();
        let names = |v: Vec<Found>| v.into_iter().map(|f| f.name).collect::<Vec<_>>();

        let d0 = crawl(&client, &site.url("/"), &GrabOptions::default(), &cancel).await.unwrap();
        assert_eq!(names(d0), ["a.zip", "doc.pdf"]);

        let d1 = crawl(&client, &site.url("/"), &GrabOptions { depth: 1, extensions: vec!["zip".into()], ..Default::default() }, &cancel).await.unwrap();
        assert_eq!(names(d1), ["a.zip", "b.zip"], "pdf filtered out, depth 2 page not visited");

        let d2 = crawl(&client, &site.url("/"), &GrabOptions { depth: 5, ..Default::default() }, &cancel).await.unwrap();
        assert_eq!(names(d2), ["a.zip", "doc.pdf", "b.zip", "c.zip"]);
        assert!(!site.hits().iter().any(|p| p.contains("elsewhere")), "other hosts are not followed");
        assert_eq!(site.hits().iter().filter(|p| p.as_str() == "/sub/deep/p3.html").count(), 1, "only the depth-5 crawl reaches the third level");
    }

    #[tokio::test]
    async fn crawl_errors_on_unreachable_or_non_html_start() {
        let mut r = Routes::new();
        r.insert("/f.bin".into(), ("application/octet-stream".into(), vec![1, 2, 3]));
        let site = serve_site(r).await;
        let client = Client::new();
        let cancel = CancellationToken::new();
        assert!(crawl(&client, &site.url("/missing"), &GrabOptions::default(), &cancel).await.is_err());
        assert!(crawl(&client, &site.url("/f.bin"), &GrabOptions::default(), &cancel).await.is_err());
        assert!(crawl(&client, "ftp://x/y", &GrabOptions::default(), &cancel).await.is_err());
    }
}
