//! Small pure helpers for the dialogs (unit-tested, no GTK).

use std::collections::HashMap;

use zdm_proto::{State, TaskInfo, Window};

/// URLs from pasted text: one per line (or whitespace separated), http/https/ftp only, duplicates dropped.
pub fn parse_urls(text: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for tok in text.split_whitespace() {
        let t = tok.trim_matches(|c| matches!(c, '<' | '>' | '"' | '\'' | ',' | ';'));
        let scheme_ok = t.starts_with("http://") || t.starts_with("https://") || t.starts_with("ftp://");
        if scheme_ok && t.len() > 8 && !out.iter().any(|u| u == t) {
            out.push(t.to_string());
        }
    }
    out
}

/// Patterns typed in a text box: separated by spaces or commas.
pub fn split_patterns(text: &str) -> Vec<String> {
    text.split(|c: char| c.is_whitespace() || c == ',').map(str::trim).filter(|p| !p.is_empty()).map(str::to_string).collect()
}

/// File types typed in a text box (`pdf, .ZIP mp3`) -> lowercase extensions without dots, letters and digits only.
pub fn parse_extensions(text: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for p in split_patterns(text) {
        let e = p.trim_start_matches('.').to_ascii_lowercase();
        if !e.is_empty() && e.len() <= 12 && e.chars().all(|c| c.is_ascii_alphanumeric()) && !out.contains(&e) {
            out.push(e);
        }
    }
    out
}

/// The pieces of a proxy address as the Options tab shows them.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProxyParts {
    /// `none`, `http`, `https` or `socks5`.
    pub kind: String,
    pub host: String,
    pub port: String,
    pub user: String,
    pub pass: String,
}

fn pct_encode(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let (mut out, mut i) = (Vec::new(), 0);
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

/// `socks5://user:pw@host:1080` -> parts (anything unreadable shows as "no proxy").
pub fn parse_proxy(url: Option<&str>) -> ProxyParts {
    let none = ProxyParts { kind: "none".into(), ..Default::default() };
    let Some(url) = url else { return none };
    let Some((scheme, rest)) = url.split_once("://") else { return none };
    let kind = match scheme {
        "http" => "http",
        "https" => "https",
        "socks5" | "socks5h" => "socks5",
        _ => return none,
    };
    let (userinfo, hostport) = match rest.rsplit_once('@') {
        Some((u, h)) => (Some(u), h),
        None => (None, rest),
    };
    let hostport = hostport.trim_end_matches('/');
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => (h, p),
        _ => (hostport, ""),
    };
    let (user, pass) = userinfo.map_or(("", ""), |u| u.split_once(':').unwrap_or((u, "")));
    ProxyParts { kind: kind.into(), host: host.into(), port: port.into(), user: pct_decode(user), pass: pct_decode(pass) }
}

/// Parts -> the address the daemon wants (`None` = use the system settings).
pub fn compose_proxy(p: &ProxyParts) -> Result<Option<String>, String> {
    if p.kind == "none" {
        return Ok(None);
    }
    let host = p.host.trim();
    if host.is_empty() || host.contains(|c: char| c.is_whitespace() || "/@".contains(c)) {
        return Err("Type the proxy's host name or address.".into());
    }
    if !p.port.is_empty() && p.port.parse::<u16>().is_err() {
        return Err("The proxy port must be a number from 1 to 65535.".into());
    }
    let auth = if p.user.is_empty() { String::new() } else if p.pass.is_empty() { format!("{}@", pct_encode(&p.user)) } else { format!("{}:{}@", pct_encode(&p.user), pct_encode(&p.pass)) };
    let port = if p.port.is_empty() { String::new() } else { format!(":{}", p.port) };
    Ok(Some(format!("{}://{auth}{host}{port}", p.kind)))
}

pub const GRAB_PRESETS: [&str; 8] = ["All files", "Documents", "Images", "Video", "Music", "Compressed", "Programs", "Custom…"];

/// Extensions for a preset of the site grabber (empty = every file-like link). `custom` is free text such as
/// `pdf, .zip  mp3` and is used for the last preset.
pub fn preset_extensions(index: usize, custom: &str) -> Vec<String> {
    let list: &[&str] = match index {
        1 => &["pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp", "txt", "epub", "rtf", "csv"],
        2 => &["jpg", "jpeg", "png", "gif", "bmp", "webp", "svg", "tif", "tiff", "ico"],
        3 => &["mp4", "mkv", "avi", "mov", "wmv", "webm", "flv", "m4v", "mpg", "mpeg", "3gp", "ogv", "m3u8", "ts"],
        4 => &["mp3", "flac", "aac", "ogg", "opus", "wav", "wma", "m4a"],
        5 => &["zip", "7z", "rar", "gz", "bz2", "xz", "zst", "tar", "tgz"],
        6 => &["exe", "msi", "deb", "rpm", "appimage", "apk", "iso", "img", "bin", "sh", "run"],
        7 => return custom.split(|c: char| c.is_whitespace() || c == ',').map(|e| e.trim_start_matches('.').to_ascii_lowercase()).filter(|e| !e.is_empty()).collect(),
        _ => &[],
    };
    list.iter().map(|s| s.to_string()).collect()
}

/// Is a copied link worth offering to download? A single link to a known file type, a stream or an FTP address
/// (copying an ordinary web page address must not trigger anything).
pub fn looks_downloadable(url: &str) -> bool {
    if url.starts_with("ftp://") {
        return true;
    }
    let path = url.split(['?', '#']).next().unwrap_or("");
    let last = path.rsplit('/').next().unwrap_or("");
    let ext = match last.rsplit_once('.') {
        Some((stem, e)) if !stem.is_empty() => e.to_ascii_lowercase(),
        _ => return false,
    };
    ["zip", "7z", "rar", "gz", "bz2", "xz", "zst", "tar", "tgz", "iso", "img", "deb", "rpm", "appimage", "apk", "exe", "msi", "bin", "run", "sh",
     "pdf", "epub", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "mp3", "flac", "ogg", "opus", "wav", "m4a", "aac",
     "mp4", "mkv", "avi", "mov", "webm", "flv", "m4v", "m3u8"]
        .contains(&ext.as_str())
}

pub const DAY_NAMES: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// `days` bit 0 = Monday ... bit 6 = Sunday (same as the daemon).
pub fn days_label(days: u8) -> String {
    if days & 0x7f == 0x7f {
        return "every day".into();
    }
    let v: Vec<&str> = (0..7).filter(|d| days & (1 << d) != 0).map(|d| DAY_NAMES[d]).collect();
    if v.is_empty() { "no days".into() } else { v.join(", ") }
}

pub fn hm(min: u32) -> (u32, u32) {
    ((min / 60).min(23), min % 60)
}

pub fn minutes(h: u32, m: u32) -> u32 {
    (h.min(23)) * 60 + m.min(59)
}

/// One-line description of a queue's schedule for the sidebar/tooltips.
pub fn window_label(w: &Option<Window>) -> String {
    match w {
        None => "runs any time".into(),
        Some(w) => {
            let (sh, sm) = hm(w.start_min);
            let (eh, em) = hm(w.end_min);
            format!("{sh:02}:{sm:02}–{eh:02}:{em:02}, {}", days_label(w.days))
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Notice {
    Done { id: i64, name: String },
    Failed { id: i64, name: String, error: String },
}

/// Which tasks deserve a desktop notification, given the states seen on the previous snapshot.
/// A task never seen before counts only if it is new (added in the last minute) and already finished.
pub fn transitions(prev: &HashMap<i64, State>, tasks: &[TaskInfo], now: i64) -> Vec<Notice> {
    let mut out = vec![];
    for t in tasks {
        let before = prev.get(&t.id).copied();
        let name = crate::fmt::display_name(t);
        match t.state {
            State::Completed if before.is_some_and(|b| b != State::Completed) || (before.is_none() && now - t.added_at < 60) => {
                out.push(Notice::Done { id: t.id, name });
            }
            State::Failed if before.is_some_and(|b| b != State::Failed) || (before.is_none() && now - t.added_at < 60) => {
                out.push(Notice::Failed { id: t.id, name, error: t.error.clone().unwrap_or_else(|| "unknown error".into()) });
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_extracted_and_deduped() {
        let t = "https://a.org/x.zip\nnot a url\n<http://b.net/y> ftp://c/z gopher://old/school\nhttps://a.org/x.zip, https://";
        assert_eq!(parse_urls(t), vec!["https://a.org/x.zip", "http://b.net/y", "ftp://c/z"]);
        assert!(parse_urls("").is_empty());
    }

    #[test]
    fn copied_links_worth_offering() {
        assert!(looks_downloadable("https://x.org/dl/tool-1.2.tar.gz?mirror=3"));
        assert!(looks_downloadable("https://x.org/a/b/movie.MP4"));
        assert!(looks_downloadable("https://v.test/live/index.m3u8"));
        assert!(looks_downloadable("ftp://mirror.test/pub/"));
        assert!(!looks_downloadable("https://example.com/"));
        assert!(!looks_downloadable("https://example.com/blog/post.html"));
        assert!(!looks_downloadable("https://example.com/.zip"));
        assert_eq!(parse_urls("ftp://m.test/f.iso http://a.b/c"), vec!["ftp://m.test/f.iso", "http://a.b/c"]);
    }

    #[test]
    fn typed_patterns_and_file_types() {
        assert_eq!(split_patterns("  */2024/*  thumb,  *.tmp "), ["*/2024/*", "thumb", "*.tmp"]);
        assert!(split_patterns("  , ").is_empty());
        assert_eq!(parse_extensions(" .PDF, zip  mp3 zip bad-ext x/y"), ["pdf", "zip", "mp3"]);
    }

    #[test]
    fn proxy_addresses_round_trip() {
        let p = parse_proxy(Some("socks5://ann:p%40ss%3Aw@127.0.0.1:1080"));
        assert_eq!(p, ProxyParts { kind: "socks5".into(), host: "127.0.0.1".into(), port: "1080".into(), user: "ann".into(), pass: "p@ss:w".into() });
        assert_eq!(compose_proxy(&p).unwrap().as_deref(), Some("socks5://ann:p%40ss%3Aw@127.0.0.1:1080"));
        assert_eq!(parse_proxy(Some("http://proxy.lan:3128")), ProxyParts { kind: "http".into(), host: "proxy.lan".into(), port: "3128".into(), ..Default::default() });
        assert_eq!(parse_proxy(Some("socks5h://h")).kind, "socks5");
        assert_eq!(parse_proxy(None).kind, "none");
        assert_eq!(parse_proxy(Some("ftp://x")).kind, "none");
        assert_eq!(compose_proxy(&ProxyParts { kind: "none".into(), host: "ignored".into(), ..Default::default() }), Ok(None));
        assert!(compose_proxy(&ProxyParts { kind: "http".into(), ..Default::default() }).is_err());
        assert!(compose_proxy(&ProxyParts { kind: "http".into(), host: "h".into(), port: "99999".into(), ..Default::default() }).is_err());
        assert_eq!(compose_proxy(&ProxyParts { kind: "https".into(), host: "h".into(), user: "u".into(), ..Default::default() }).unwrap().as_deref(), Some("https://u@h"));
    }

    #[test]
    fn grabber_presets() {
        assert!(preset_extensions(0, "").is_empty());
        assert!(preset_extensions(3, "").contains(&"m3u8".to_string()));
        assert_eq!(preset_extensions(7, " .PDF, zip\tmp3 ,, "), ["pdf", "zip", "mp3"]);
        assert!(preset_extensions(99, "x").is_empty());
    }

    #[test]
    fn day_and_time_labels() {
        assert_eq!(days_label(0x7f), "every day");
        assert_eq!(days_label(0b0011111), "Mon, Tue, Wed, Thu, Fri");
        assert_eq!(days_label(0), "no days");
        assert_eq!(hm(23 * 60 + 5), (23, 5));
        assert_eq!(minutes(7, 30), 450);
        assert_eq!(window_label(&None), "runs any time");
        assert_eq!(window_label(&Some(Window { start_min: 60, end_min: 420, days: 0x7f })), "01:00–07:00, every day");
    }

    fn task(id: i64, state: State, added_at: i64) -> TaskInfo {
        TaskInfo { id, url: format!("http://x/{id}.zip"), filename: None, dest: None, category: None, queue: "Main".into(), state, size: None, downloaded: 0, error: Some("boom".into()), added_at }
    }

    #[test]
    fn notices_only_on_a_change_to_finished_or_failed() {
        let prev: HashMap<i64, State> = [(1, State::Running), (2, State::Completed), (3, State::Running), (4, State::Failed)].into();
        let tasks = [task(1, State::Completed, 0), task(2, State::Completed, 0), task(3, State::Failed, 0), task(4, State::Failed, 0), task(5, State::Running, 0)];
        let n = transitions(&prev, &tasks, 1000);
        assert_eq!(n.len(), 2);
        assert!(matches!(&n[0], Notice::Done { id: 1, name } if name == "http://x/1.zip"));
        assert!(matches!(&n[1], Notice::Failed { id: 3, error, .. } if error == "boom"));
    }

    #[test]
    fn unseen_task_notifies_only_if_new() {
        let prev = HashMap::new();
        assert_eq!(transitions(&prev, &[task(9, State::Completed, 990)], 1000).len(), 1);
        assert!(transitions(&prev, &[task(9, State::Completed, 100)], 1000).is_empty(), "old history on startup must stay quiet");
    }
}
