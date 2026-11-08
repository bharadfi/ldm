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

pub fn ytdlp_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("ZDM_YTDLP") {
        return (!p.is_empty() && p != "none").then(|| PathBuf::from(p));
    }
    let on_path = std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join("yt-dlp")).find(|f| f.is_file()));
    on_path.or_else(|| dirs::home_dir().map(|h| h.join(".local/bin/yt-dlp")).filter(|f| f.is_file()))
}

fn need_ytdlp() -> Result<PathBuf> {
    ytdlp_path().ok_or_else(|| anyhow!("video pages need yt-dlp, which is not installed (see https://github.com/yt-dlp/yt-dlp#installation)"))
}

/// A selector from the user's menu is passed to yt-dlp as a single argument; still, accept only the characters
/// that format selectors and our own `x:` / `sub:` forms use.
pub fn valid_selector(s: &str) -> bool {
    !s.is_empty() && s.len() <= 200 && s.chars().all(|c| c.is_ascii_alphanumeric() || "+-_.[]=/,<>:!*()".contains(c)) && !s.starts_with('-')
}

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
pub async fn list_formats(url: &str, proxy: Option<&str>) -> Result<FormatList> {
    let exe = need_ytdlp()?;
    let mut cmd = tokio::process::Command::new(exe);
    cmd.args(["-J", "--no-playlist", "--no-warnings", "--no-colors"]);
    if let Some(p) = proxy {
        cmd.args(["--proxy", p]);
    }
    cmd.arg("--").arg(url).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(90), cmd.output()).await.map_err(|_| anyhow!("the site took too long to answer"))?.context("could not start yt-dlp")?;
    if !out.status.success() {
        let e = String::from_utf8_lossy(&out.stderr);
        let line = e.lines().rev().find(|l| l.contains("ERROR")).or_else(|| e.lines().last()).unwrap_or("unknown error");
        bail!("yt-dlp could not read this page: {}", line.trim_start_matches("ERROR: ").trim());
    }
    let v: Value = serde_json::from_slice(&out.stdout).context("yt-dlp gave an unreadable answer")?;
    let list = curate(&v);
    if list.options.is_empty() {
        bail!("no downloadable video or audio was found on this page");
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
    for (k, v) in job.headers {
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
        bail!("yt-dlp could not download this: {}", line.trim_start_matches("ERROR: ").trim());
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
