//! Video pages (YouTube and hundreds of other sites) through the open-source `yt-dlp` program:
//! it knows how to find the real streams; we turn its long format list into a short menu and run the download.
//!
//! `yt-dlp` is found through `ZDM_YTDLP` (tests), then `PATH`, then `~/.local/bin`. It needs updating from time
//! to time because sites change (`yt-dlp -U`); `ffmpeg` joins separate video and audio streams.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio_util::sync::CancellationToken;
use zdm_core::Cancelled;
use zdm_proto::{FormatList, FormatOption};

/// Sortable numbers of a yt-dlp version (`2026.10.26`, `2025.01.15.232754`): newer is bigger.
pub fn version_key(v: &str) -> Vec<u64> {
    v.trim().split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty()).filter_map(|p| p.parse().ok()).collect()
}

/// The newest of the installed copies. Several can exist (the distribution's old one in /usr/bin, one
/// downloaded into ~/.local/bin) and which one a program finds depends on its PATH, so look at all of them.
pub fn newest(candidates: Vec<(PathBuf, String)>) -> Option<(PathBuf, String)> {
    candidates.into_iter().max_by(|a, b| version_key(&a.1).cmp(&version_key(&b.1)))
}

fn run_version(exe: &Path) -> Option<String> {
    let mut child = std::process::Command::new(exe).arg("--version").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let start = std::time::Instant::now();
    loop {
        match child.try_wait().ok()? {
            Some(st) if st.success() => {
                let mut s = String::new();
                std::io::Read::read_to_string(child.stdout.as_mut()?, &mut s).ok()?;
                return Some(s.trim().to_string()).filter(|v| !v.is_empty());
            }
            Some(_) => return None,
            None if start.elapsed() > Duration::from_secs(10) => {
                let _ = child.kill();
                return None;
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

fn candidate_paths() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = vec![];
    if let Some(h) = dirs::home_dir() {
        dirs.push(h.join(".local/bin"));
    }
    if let Some(p) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&p));
    }
    dirs.extend(["/usr/local/bin", "/usr/bin", "/snap/bin"].map(PathBuf::from));
    let mut out: Vec<PathBuf> = vec![];
    for d in dirs {
        let f = d.join("yt-dlp");
        // two directories can lead to the same program (/bin -> /usr/bin): ask each program once
        let real = std::fs::canonicalize(&f).unwrap_or_else(|_| f.clone());
        if f.is_file() && !out.iter().any(|o| std::fs::canonicalize(o).unwrap_or_else(|_| o.clone()) == real) {
            out.push(f);
        }
    }
    out
}

type Found = Option<(PathBuf, String)>;

/// (program, version) of the newest installed yt-dlp; looked up at most every ten minutes.
pub fn ytdlp_info() -> Found {
    static CACHE: std::sync::Mutex<Option<(std::time::Instant, Found)>> = std::sync::Mutex::new(None);
    if let Ok(p) = std::env::var("ZDM_YTDLP") {
        // tests (and anyone who wants to force one) name the program; it is not asked for its version
        return (!p.is_empty() && p != "none").then(|| (PathBuf::from(p), "(chosen)".to_string()));
    }
    let mut cache = CACHE.lock().unwrap();
    if let Some((at, found)) = &*cache {
        if at.elapsed() < Duration::from_secs(600) && found.is_some() {
            return found.clone();
        }
    }
    let found = newest(candidate_paths().into_iter().filter_map(|p| run_version(&p).map(|v| (p, v))).collect());
    *cache = Some((std::time::Instant::now(), found.clone()));
    found
}

pub fn ytdlp_path() -> Option<PathBuf> {
    ytdlp_info().map(|(p, _)| p)
}

/// Plain words for yt-dlp's well-known refusals; `None` leaves the original text.
pub fn friendly(line: &str) -> Option<String> {
    let l = line.to_ascii_lowercase();
    if l.contains("sign in to confirm") || l.contains("not a bot") {
        return Some("YouTube is asking for proof that this is not a robot. It does this when a computer makes many automatic requests, and usually lets go after a few hours. The browser button sends your browser's own cookies, which normally avoids it; for the app's own \"Download video from a web page\", choose a browser in Options > Downloads > \"Use the cookies of this browser\"".to_string());
    }
    if l.contains("video unavailable") || l.contains("private video") {
        return Some(line.trim().to_string());
    }
    None
}

/// Netscape cookie file content for a browser `Cookie` header (`a=b; c=d`) of a page on `host`. The cookies are
/// given to the registrable domain (`www.youtube.com` -> `.youtube.com`), which is where sites keep them.
pub fn netscape_cookies(host: &str, cookie_header: &str, secure: bool, expires: u64) -> String {
    let labels: Vec<&str> = host.trim_start_matches('.').split('.').collect();
    let keep = if labels.len() >= 3 && labels[labels.len() - 1].len() == 2 && ["co", "com", "org", "net", "gov", "ac"].contains(&labels[labels.len() - 2]) { 3 } else { 2 };
    let domain = format!(".{}", labels[labels.len().saturating_sub(keep)..].join("."));
    let mut out = String::from("# Netscape HTTP Cookie File\n");
    for pair in cookie_header.split(';') {
        let Some((name, value)) = pair.trim().split_once('=') else { continue };
        if name.is_empty() || name.contains(['\t', '\n', '\r']) || value.contains(['\t', '\n', '\r']) {
            continue;
        }
        out += &format!("{domain}\tTRUE\t/\t{}\t{expires}\t{name}\t{value}\n", if secure { "TRUE" } else { "FALSE" });
    }
    out
}

/// A private (0600) cookie file for yt-dlp that is deleted when dropped.
struct CookieFile(PathBuf);

impl CookieFile {
    fn create(page_url: &str, cookie_header: &str) -> Option<CookieFile> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let u = url::Url::parse(page_url).ok()?;
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos();
        let path = std::env::temp_dir().join(format!("zdm-cookies-{}-{nanos}.txt", std::process::id()));
        let expires = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() + 365 * 86400;
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path).ok()?;
        f.write_all(netscape_cookies(u.host_str()?, cookie_header, u.scheme() == "https", expires).as_bytes()).ok()?;
        Some(CookieFile(path))
    }
}

impl Drop for CookieFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Cookie arguments for yt-dlp: the browser's cookies for this page when we have them, else the browser
/// the user chose in Options. The returned guard keeps the temporary file alive.
fn cookie_args(cmd: &mut tokio::process::Command, page_url: &str, cookie: Option<&str>, browser: Option<&str>) -> Option<CookieFile> {
    if let Some(file) = cookie.filter(|c| !c.trim().is_empty()).and_then(|c| CookieFile::create(page_url, c)) {
        cmd.arg("--cookies").arg(&file.0);
        return Some(file);
    }
    if let Some(b) = browser {
        cmd.args(["--cookies-from-browser", b]);
    }
    None
}

fn version_note() -> String {
    ytdlp_info().map(|(_, v)| format!(" (yt-dlp {v})")).unwrap_or_default()
}

fn need_ytdlp() -> Result<PathBuf> {
    ytdlp_path().ok_or_else(|| anyhow!("video pages need yt-dlp, which is not installed (see https://github.com/yt-dlp/yt-dlp#installation)"))
}

pub use zdm_proto::valid_ytdlp_selector as valid_selector;

fn sanitize(title: &str) -> String {
    let t: String = title.chars().map(|c| if c.is_control() || "/\\:*?\"<>|".contains(c) { ' ' } else { c }).collect();
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    let t: String = t.chars().take(120).collect();
    let t = t.trim_matches(|c: char| c == '.' || c.is_whitespace()).to_string();
    if t.is_empty() { "video".into() } else { t }
}

#[derive(Debug, Clone)]
struct Fmt {
    id: String,
    ext: String,
    height: Option<u32>,
    fps: f64,
    vcodec: String,
    acodec: String,
    size: Option<u64>,
    abr: f64,
    https: bool,
}

impl Fmt {
    fn video_only(&self) -> bool {
        self.vcodec != "none" && self.acodec == "none"
    }
    fn audio_only(&self) -> bool {
        self.vcodec == "none" && self.acodec != "none" && !self.id.contains("drc")
    }
    fn progressive(&self) -> bool {
        self.vcodec != "none" && self.acodec != "none"
    }
    /// Lower is better: what plays on the most devices first (H.264, then VP9, then AV1).
    fn codec_rank(&self) -> u8 {
        if self.vcodec.starts_with("avc1") || self.vcodec.starts_with("h264") { 0 } else if self.vcodec.starts_with("vp") { 1 } else if self.vcodec.starts_with("av01") { 2 } else { 3 }
    }
}

fn parse_fmt(f: &Value) -> Option<Fmt> {
    let s = |k: &str| f.get(k).and_then(Value::as_str).unwrap_or("none").to_string();
    let ext = s("ext");
    if ext == "mhtml" || ext == "none" {
        return None; // storyboards
    }
    let protocol = s("protocol");
    Some(Fmt {
        id: f.get("format_id").and_then(Value::as_str)?.to_string(),
        ext,
        height: f.get("height").and_then(Value::as_u64).map(|h| h as u32),
        fps: f.get("fps").and_then(Value::as_f64).unwrap_or(0.0),
        vcodec: s("vcodec"),
        acodec: s("acodec"),
        size: f.get("filesize").and_then(Value::as_u64).or_else(|| f.get("filesize_approx").and_then(Value::as_u64)),
        abr: f.get("abr").and_then(Value::as_f64).or_else(|| f.get("tbr").and_then(Value::as_f64)).unwrap_or(0.0),
        https: protocol.starts_with("http") && !protocol.contains("m3u8") && !protocol.contains("dash"),
    })
}

fn quality_label(height: u32, fps: f64) -> String {
    let hd = if height >= 2160 { " 4K" } else if height >= 720 { " HD" } else { "" };
    let fps = if fps > 31.0 { format!(" {}fps", fps.round() as u32) } else { String::new() };
    format!("{height}p{hd}{fps}")
}

/// yt-dlp's `-J` answer -> the short menu: one MP4 (or WEBM) per resolution, the best audio tracks, subtitles.
pub fn curate(v: &Value) -> FormatList {
    let title = v.get("title").and_then(Value::as_str).unwrap_or("video").to_string();
    let base = sanitize(&title);
    let all: Vec<Fmt> = v.get("formats").and_then(Value::as_array).map(|a| a.iter().filter_map(parse_fmt).collect()).unwrap_or_default();
    // direct https files are preferred; stream playlists only when nothing else exists
    let direct: Vec<Fmt> = if all.iter().any(|f| f.https) { all.iter().filter(|f| f.https).cloned().collect() } else { all.clone() };

    let best_audio = |ext: &str| direct.iter().filter(|f| f.audio_only() && f.ext == ext).max_by(|a, b| a.abr.partial_cmp(&b.abr).unwrap_or(std::cmp::Ordering::Equal));
    let (m4a, opus) = (best_audio("m4a"), best_audio("webm"));
    let mut options = vec![];

    let mut heights: Vec<u32> = direct.iter().filter(|f| f.video_only() || f.progressive()).filter_map(|f| f.height).collect();
    heights.sort_unstable_by(|a, b| b.cmp(a));
    heights.dedup();
    for h in heights {
        let pick = direct.iter().filter(|f| (f.video_only() || f.progressive()) && f.height == Some(h)).min_by(|a, b| {
            a.codec_rank().cmp(&b.codec_rank()).then(b.fps.partial_cmp(&a.fps).unwrap_or(std::cmp::Ordering::Equal)).then(b.size.cmp(&a.size))
        });
        let Some(p) = pick else { continue };
        let (id, ext, size) = if p.progressive() {
            (p.id.clone(), p.ext.clone(), p.size)
        } else {
            // pair the video with the audio that fits its container: m4a for MP4, opus for WEBM
            let audio = if p.ext == "mp4" { m4a.or(opus) } else { opus.or(m4a) };
            match audio {
                Some(a) => (format!("{}+{}", p.id, a.id), if p.ext == "mp4" && a.ext == "m4a" { "mp4".into() } else if p.ext == "webm" && a.ext == "webm" { "webm".into() } else { "mkv".into() }, p.size.zip(a.size).map(|(x, y)| x + y).or(p.size)),
                None => (p.id.clone(), p.ext.clone(), p.size),
            }
        };
        options.push(FormatOption {
            filename: format!("{base} [{h}p].{ext}"),
            label: format!("{} file, quality {}", ext.to_uppercase(), quality_label(h, p.fps)),
            id,
            kind: "video".into(),
            ext,
            height: Some(h),
            size,
        });
    }
    for (a, name, ext) in [(m4a, "M4A", "m4a"), (opus, "WEBM (Opus)", "webm")] {
        if let Some(a) = a {
            options.push(FormatOption { id: a.id.clone(), kind: "audio".into(), label: format!("{name} audio only, {:.0} kbps", a.abr), ext: ext.into(), height: None, size: a.size, filename: format!("{base} [audio].{ext}") });
        }
    }
    if m4a.is_some() || opus.is_some() {
        options.push(FormatOption { id: "x:mp3".into(), kind: "audio".into(), label: "MP3 audio only (converted)".into(), ext: "mp3".into(), height: None, size: None, filename: format!("{base} [audio].mp3") });
    }
    // subtitles: the ones people wrote, and the automatic ones in the video's own language
    let langs = |k: &str| v.get(k).and_then(Value::as_object).map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default();
    for l in langs("subtitles").into_iter().take(8) {
        options.push(FormatOption { id: format!("sub:{l}"), kind: "subtitle".into(), label: format!("SRT subtitles, {l}"), ext: "srt".into(), height: None, size: None, filename: format!("{base} [{l}].srt") });
    }
    let own = v.get("language").and_then(Value::as_str).unwrap_or("en").to_string();
    if langs("automatic_captions").contains(&own) && !langs("subtitles").contains(&own) {
        options.push(FormatOption { id: format!("sub-auto:{own}"), kind: "subtitle".into(), label: format!("SRT subtitles, {own} (automatic)"), ext: "srt".into(), height: None, size: None, filename: format!("{base} [{own} auto].srt") });
    }
    FormatList { title, duration: v.get("duration").and_then(Value::as_f64), options }
}

/// Ask yt-dlp what the page at `url` offers.
pub async fn list_formats(url: &str, proxy: Option<&str>, cookie: Option<&str>, cookie_browser: Option<&str>) -> Result<FormatList> {
    // the same page asked again soon (a second click, the app after the browser) is answered without
    // bothering the site: many automatic requests are what makes sites distrust a computer
    static CACHE: std::sync::Mutex<Vec<(String, std::time::Instant, FormatList)>> = std::sync::Mutex::new(Vec::new());
    let key = format!("{url}|{}", cookie.is_some());
    if let Some((_, _, l)) = CACHE.lock().unwrap().iter().find(|(k, at, _)| *k == key && at.elapsed() < Duration::from_secs(600)) {
        return Ok(l.clone());
    }
    let list = list_formats_uncached(url, proxy, cookie, cookie_browser).await?;
    let mut c = CACHE.lock().unwrap();
    c.retain(|(k, at, _)| *k != key && at.elapsed() < Duration::from_secs(600));
    c.push((key, std::time::Instant::now(), list.clone()));
    if c.len() > 40 {
        c.remove(0);
    }
    Ok(list)
}

async fn list_formats_uncached(url: &str, proxy: Option<&str>, cookie: Option<&str>, cookie_browser: Option<&str>) -> Result<FormatList> {
    match list_formats_once(url, proxy, cookie, cookie_browser).await {
        // Signed-in requests sometimes see no usable formats where an anonymous one does (seen on finished
        // live streams): try once more without cookies before giving up.
        Err(e) if (cookie.is_some() || cookie_browser.is_some()) && format!("{e:#}").contains("Requested format is not available") => {
            list_formats_once(url, proxy, None, None).await.map_err(|_| e)
        }
        other => other,
    }
}

async fn list_formats_once(url: &str, proxy: Option<&str>, cookie: Option<&str>, cookie_browser: Option<&str>) -> Result<FormatList> {
    let exe = need_ytdlp()?;
    let mut cmd = tokio::process::Command::new(exe);
    cmd.args(["-J", "--no-playlist", "--no-warnings", "--no-colors"]);
    if let Some(p) = proxy {
        cmd.args(["--proxy", p]);
    }
    let _cookie_guard = cookie_args(&mut cmd, url, cookie, cookie_browser);
    cmd.arg("--").arg(url).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(90), cmd.output()).await.map_err(|_| anyhow!("the site took too long to answer"))?.context("could not start yt-dlp")?;
    if !out.status.success() {
        let e = String::from_utf8_lossy(&out.stderr);
        let line = e.lines().rev().find(|l| l.contains("ERROR")).or_else(|| e.lines().last()).unwrap_or("unknown error");
        let reason = line.trim_start_matches("ERROR: ").trim();
        match friendly(reason) {
            Some(f) => bail!("{f}"),
            None => bail!("yt-dlp{} could not read this page: {reason}. If this keeps happening, update yt-dlp with `yt-dlp -U`.", version_note()),
        }
    }
    let v: Value = serde_json::from_slice(&out.stdout).context("yt-dlp gave an unreadable answer")?;
    if v.is_null() {
        // an outdated yt-dlp often answers "null" for sites that changed; say so instead of "nothing found"
        bail!("yt-dlp{} could not read this page; it is probably out of date: update it with `yt-dlp -U` (or download the newest from github.com/yt-dlp/yt-dlp)", version_note());
    }
    let list = curate(&v);
    if list.options.is_empty() {
        bail!("yt-dlp{} found no video or audio to download on this page", version_note());
    }
    Ok(list)
}

/// Extra arguments for a menu id: what to fetch and how to save it.
fn selector_args(sel: &str) -> Vec<String> {
    let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    if let Some(codec) = sel.strip_prefix("x:") {
        return v(&["-x", "--audio-format", codec, "-f", "bestaudio"]);
    }
    if let Some(lang) = sel.strip_prefix("sub:") {
        return v(&["--skip-download", "--write-subs", "--sub-langs", lang, "--convert-subs", "srt"]);
    }
    if let Some(lang) = sel.strip_prefix("sub-auto:") {
        return v(&["--skip-download", "--write-auto-subs", "--sub-langs", lang, "--convert-subs", "srt"]);
    }
    v(&["-f", sel])
}

/// Progress line from our template: `ZDMP <downloaded> <total> <estimate>` (fields may be `NA`).
pub fn parse_progress(line: &str) -> Option<(u64, u64)> {
    let rest = line.trim().strip_prefix("ZDMP ")?;
    let mut it = rest.split_whitespace().map(|x| x.parse::<f64>().ok());
    let done = it.next()??;
    let total = it.next().flatten().or_else(|| it.next().flatten()).unwrap_or(0.0);
    Some((done as u64, total as u64))
}

pub struct YtdlpJob<'a> {
    pub page_url: &'a str,
    pub selector: &'a str,
    /// The planned file; the real one may get another extension (returned).
    pub dest: &'a Path,
    pub proxy: Option<&'a str>,
    pub referer: Option<&'a str>,
    pub user_agent: Option<&'a str>,
    pub headers: &'a [(String, String)],
    /// KiB/s, 0 = unlimited.
    pub limit_kib: u64,
    /// Browser whose cookies yt-dlp may use when the task has none of its own (from Options).
    pub cookie_browser: Option<&'a str>,
    pub cancel: &'a CancellationToken,
}

/// Fetch one menu choice into `dest`. Partial files are kept in `<dest>.ytdlp/` so a paused download resumes.
/// `on_progress(done, estimated_total)` counts all streams together. Returns the finished file.
pub async fn download(job: YtdlpJob<'_>, on_progress: impl Fn(u64, u64) + Send + Sync + 'static) -> Result<PathBuf> {
    let exe = need_ytdlp()?;
    if !valid_selector(job.selector) {
        bail!("unsupported format selection");
    }
    let mut work = job.dest.as_os_str().to_owned();
    work.push(".ytdlp");
    let work = PathBuf::from(work);
    std::fs::create_dir_all(&work)?;

    let mut cmd = tokio::process::Command::new(exe);
    cmd.args(["--no-playlist", "--no-warnings", "--no-colors", "--newline", "--no-mtime", "--progress-template", "download:ZDMP %(progress.downloaded_bytes)s %(progress.total_bytes)s %(progress.total_bytes_estimate)s"]);
    cmd.args(selector_args(job.selector));
    let want = job.dest.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    if ["mp4", "mkv", "webm"].contains(&want.as_str()) && !job.selector.starts_with("sub") {
        cmd.args(["--merge-output-format", &want]);
    }
    if job.limit_kib > 0 {
        cmd.args(["--limit-rate", &format!("{}K", job.limit_kib)]);
    }
    if let Some(p) = job.proxy {
        cmd.args(["--proxy", p]);
    }
    if let Some(r) = job.referer {
        cmd.args(["--referer", r]);
    }
    if let Some(ua) = job.user_agent {
        cmd.args(["--user-agent", ua]);
    }
    // the browser's Cookie header becomes a private cookie file (yt-dlp keeps its own cookie jar; a plain header is
    // ignored by some sites); other headers are passed as they are
    let cookie = job.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("cookie")).map(|(_, v)| v.as_str());
    let _cookie_guard = cookie_args(&mut cmd, job.page_url, cookie, job.cookie_browser);
    for (k, v) in job.headers.iter().filter(|(k, _)| !k.eq_ignore_ascii_case("cookie")) {
        cmd.args(["--add-headers", &format!("{k}:{v}")]);
    }
    cmd.arg("-o").arg(work.join("out.%(ext)s")).arg("--").arg(job.page_url);
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let mut child = cmd.spawn().context("could not start yt-dlp")?;
    let mut lines = BufReader::new(child.stdout.take().expect("piped")).lines();
    let mut stderr = child.stderr.take().expect("piped");
    let err = tokio::spawn(async move {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s).await;
        s
    });

    // several streams (video, then audio) are fetched one after the other: add them up
    let (mut base, mut last_done, mut last_total) = (0u64, 0u64, 0u64);
    let status = loop {
        tokio::select! {
            _ = job.cancel.cancelled() => {
                let _ = child.kill().await;
                return Err(Cancelled.into());
            }
            line = lines.next_line() => match line {
                Ok(Some(l)) => {
                    if let Some((done, total)) = parse_progress(&l) {
                        if done + 1024 < last_done {
                            base += last_total.max(last_done);
                        }
                        (last_done, last_total) = (done, total.max(done));
                        on_progress(base + done, if total > 0 { base + total } else { 0 });
                    }
                }
                _ => break child.wait().await?,
            },
        }
    };
    if !status.success() {
        let e = err.await.unwrap_or_default();
        let line = e.lines().rev().find(|l| l.contains("ERROR")).or_else(|| e.lines().last()).unwrap_or("it stopped without a message");
        let reason = line.trim_start_matches("ERROR: ").trim();
        match friendly(reason) {
            Some(f) => bail!("{f}"),
            None => bail!("yt-dlp could not download this: {reason}"),
        }
    }
    // the finished file is the biggest `out.*` that is not a partial or bookkeeping leftover (when yt-dlp joins
    // video and audio it deletes the separate streams itself)
    let mut best: Option<(u64, PathBuf)> = None;
    for e in std::fs::read_dir(&work)? {
        let p = e?.path();
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !name.starts_with("out.") || name.ends_with(".part") || name.ends_with(".ytdl") {
            continue;
        }
        let len = std::fs::metadata(&p)?.len();
        if best.as_ref().is_none_or(|(l, _)| len > *l) {
            best = Some((len, p));
        }
    }
    let (len, produced) = best.ok_or_else(|| anyhow!("yt-dlp finished but produced no file"))?;
    if len == 0 {
        bail!("yt-dlp produced an empty file");
    }
    // keep the planned name, with the extension that was really produced
    let real_ext = produced.extension().and_then(|e| e.to_str()).unwrap_or("").to_string();
    let target = if real_ext.is_empty() || real_ext.eq_ignore_ascii_case(&want) { job.dest.to_path_buf() } else { job.dest.with_extension(&real_ext) };
    std::fs::rename(&produced, &target).or_else(|_| std::fs::copy(&produced, &target).map(drop))?;
    let _ = std::fs::remove_dir_all(&work);
    on_progress(len, len);
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/ytdlp-formats.json")).unwrap()
    }

    #[test]
    fn long_format_list_becomes_a_short_menu() {
        let l = curate(&fixture());
        assert_eq!(l.title, "Big Buck Bunny 60fps 4K - Official Blender Foundation Short Film");
        let video: Vec<&FormatOption> = l.options.iter().filter(|o| o.kind == "video").collect();
        // highest first, one entry per resolution, no storyboards
        let heights: Vec<u32> = video.iter().filter_map(|o| o.height).collect();
        let mut sorted = heights.clone();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(heights, sorted);
        assert!(heights.windows(2).all(|w| w[0] != w[1]) && heights.contains(&1080) && heights.contains(&360) && !heights.contains(&27), "{heights:?}");
        let v1080 = video.iter().find(|o| o.height == Some(1080)).unwrap();
        // H.264 video-only 299 + the best m4a audio 140, joined into an MP4
        assert_eq!(v1080.id, "299+140");
        assert_eq!(v1080.ext, "mp4");
        assert_eq!(v1080.label, "MP4 file, quality 1080p HD 60fps");
        assert_eq!(v1080.size, Some(257_619_653 + 10_271_496));
        assert_eq!(v1080.filename, "Big Buck Bunny 60fps 4K - Official Blender Foundation Short Film [1080p].mp4");
        // 2160p only exists as vp9/av01: a WEBM with opus audio
        let v2160 = video.iter().find(|o| o.height == Some(2160)).unwrap();
        assert_eq!((v2160.id.as_str(), v2160.ext.as_str()), ("315+251", "webm"));
        assert!(v2160.label.contains("4K"));
        // a progressive file is used as it is
        let v360 = video.iter().find(|o| o.height == Some(360)).unwrap();
        assert_eq!(v360.id, "18");
        assert_eq!(v360.size, Some(21_000_000));
    }

    #[test]
    fn audio_and_subtitle_choices() {
        let l = curate(&fixture());
        let ids = |k: &str| l.options.iter().filter(|o| o.kind == k).map(|o| o.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids("audio"), ["140", "251", "x:mp3"], "best m4a, best opus (no DRC variants), converted mp3");
        assert_eq!(ids("subtitle"), ["sub:en", "sub:hi"].map(String::from), "the video's own language is already written by hand, so no automatic duplicate");
        let mut v = fixture();
        v["subtitles"] = serde_json::json!({});
        let auto: Vec<String> = curate(&v).options.iter().filter(|o| o.kind == "subtitle").map(|o| o.id.clone()).collect();
        assert_eq!(auto, ["sub-auto:en"]);
        assert!(curate(&serde_json::json!({"title": "x", "formats": []})).options.is_empty());
    }

    #[test]
    fn the_newest_copy_wins_whatever_the_search_path() {
        assert!(version_key("2026.10.26") > version_key("2024.04.09"));
        assert!(version_key("2025.01.15.232754") > version_key("2025.01.15"));
        assert!(version_key("2025.10.02") > version_key("2025.9.30"), "numbers, not text: 10 > 9");
        let best = newest(vec![
            (PathBuf::from("/usr/bin/yt-dlp"), "2024.04.09".into()),
            (PathBuf::from("/home/me/.local/bin/yt-dlp"), "2026.10.26".into()),
            (PathBuf::from("/usr/local/bin/yt-dlp"), "2025.06.01".into()),
        ])
        .unwrap();
        assert_eq!(best.0, PathBuf::from("/home/me/.local/bin/yt-dlp"));
        assert!(newest(vec![]).is_none());
    }

    #[test]
    fn cookie_file_and_friendly_messages() {
        let f = netscape_cookies("www.youtube.com", "VISITOR=abc; CONSENT=yes; broken; =novalue; PREF=f6=40000000&tz=Asia", true, 1_900_000_000);
        let lines: Vec<&str> = f.lines().collect();
        assert_eq!(lines[0], "# Netscape HTTP Cookie File");
        assert_eq!(lines[1], ".youtube.com\tTRUE\t/\tTRUE\t1900000000\tVISITOR\tabc");
        assert_eq!(lines.len(), 4, "two good cookies and one with '=' inside its value; malformed pieces are skipped: {f}");
        assert!(lines[3].ends_with("PREF\tf6=40000000&tz=Asia"));
        assert!(netscape_cookies("news.bbc.co.uk", "a=b", false, 1).contains(".bbc.co.uk\tTRUE\t/\tFALSE\t1\ta\tb"), "two-part public suffixes keep three labels");
        assert!(netscape_cookies("localhost", "a=b", false, 1).contains(".localhost"));
        assert!(friendly("[youtube] x: Sign in to confirm you’re not a bot. Use --cookies-from-browser").unwrap().contains("not a robot"));
        assert!(friendly("ERROR: Video unavailable").is_some());
        assert!(friendly("HTTP Error 404").is_none());
    }

    #[test]
    fn helpers() {
        assert!(valid_selector("299+140") && valid_selector("bestvideo[height<=720]+bestaudio/best") && valid_selector("x:mp3") && valid_selector("sub-auto:en"));
        assert!(!valid_selector("") && !valid_selector("--exec") && !valid_selector("a b") && !valid_selector("a;rm") && !valid_selector(&"x".repeat(300)));
        assert_eq!(sanitize("  A/B: \"C\"?*  <d>. "), "A B C d");
        assert_eq!(sanitize("???"), "video");
        assert_eq!(parse_progress("ZDMP 1500 3000 NA"), Some((1500, 3000)));
        assert_eq!(parse_progress("ZDMP 1500 NA 4000"), Some((1500, 4000)));
        assert_eq!(parse_progress("ZDMP 1500 NA NA"), Some((1500, 0)));
        assert_eq!(parse_progress("[download] 10%"), None);
        assert_eq!(selector_args("x:mp3"), ["-x", "--audio-format", "mp3", "-f", "bestaudio"]);
        assert_eq!(selector_args("299+140"), ["-f", "299+140"]);
        assert!(selector_args("sub:hi").contains(&"hi".to_string()));
        assert_eq!(quality_label(720, 30.0), "720p HD");
        assert_eq!(quality_label(480, 25.0), "480p");
    }
}
