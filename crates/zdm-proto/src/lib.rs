//! Wire types and client for talking to the `zdmd` daemon.
//!
//! Protocol: one JSON object per line, request then response, over a Unix socket (Linux/macOS) or an
//! authenticated loopback TCP connection (Windows development builds: the first line is the token from
//! `daemon.json`).

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Waiting for a free slot in its queue (or for the queue's time window).
    Queued,
    Running,
    Paused,
    Completed,
    Failed,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Queued => "queued",
            State::Running => "running",
            State::Paused => "paused",
            State::Completed => "completed",
            State::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<State> {
        Some(match s {
            "queued" => State::Queued,
            "running" => State::Running,
            "paused" => State::Paused,
            "completed" => State::Completed,
            "failed" => State::Failed,
            _ => return None,
        })
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct AddRequest {
    pub url: String,
    /// Override the server-suggested file name.
    pub filename: Option<String>,
    /// Override the category folder (absolute path).
    pub dest_dir: Option<String>,
    /// Queue name; the default queue when absent.
    pub queue: Option<String>,
    pub referer: Option<String>,
    /// User-Agent of the browser that started the download (cookies are often tied to it).
    #[serde(default)]
    pub user_agent: Option<String>,
    /// Extra request headers such as `Cookie`. Stored in the daemon database so downloads can resume.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// Queue it for immediate start (default) or add it paused.
    #[serde(default = "yes")]
    pub start: bool,
    /// Add another task even if the same link is already queued, running, paused or failed
    /// (by default the existing task is returned, and resumed if it was stopped).
    #[serde(default)]
    pub allow_duplicate: bool,
    /// For a video page: what to fetch with yt-dlp, as given by a `FormatOption::id` from `Request::Formats`.
    /// The task's link is then the page address, and `filename` should be the option's suggested name.
    #[serde(default)]
    pub ytdlp: Option<String>,
    /// Name and password for sites that ask for Basic or Digest login. (`http://user:pass@host/…` links work too.)
    /// Kept in the daemon's private database, never shown in the task list.
    #[serde(default)]
    pub auth: Option<(String, String)>,
}

fn yes() -> bool {
    true
}

/// Weekly time window in local time during which a queue may run.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    /// Minutes after local midnight, 0..1440.
    pub start_min: u32,
    pub end_min: u32,
    /// Bit 0 = Monday ... bit 6 = Sunday.
    pub days: u8,
}

impl Window {
    /// `weekday` is 0 = Monday ... 6 = Sunday. A window with `end < start` runs overnight and belongs to
    /// the day it starts on; `start == end` means all day.
    pub fn active_at(&self, minute: u32, weekday: u32) -> bool {
        let day = |d: u32| self.days & (1 << (d % 7)) != 0;
        if self.start_min == self.end_min {
            return day(weekday);
        }
        if self.start_min < self.end_min {
            return day(weekday) && minute >= self.start_min && minute < self.end_min;
        }
        (day(weekday) && minute >= self.start_min) || (day(weekday + 6) && minute < self.end_min)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct QueueInfo {
    pub name: String,
    /// Maximum tasks of this queue downloading at once.
    pub max_parallel: u32,
    pub window: Option<Window>,
    /// Speed limit shared by the running downloads of this queue in KiB/s; 0 = none.
    #[serde(default)]
    pub limit_kib: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TaskInfo {
    pub id: i64,
    pub url: String,
    pub filename: Option<String>,
    /// Final path once chosen (after the first start).
    pub dest: Option<String>,
    pub category: Option<String>,
    pub queue: String,
    pub state: State,
    pub size: Option<u64>,
    pub downloaded: u64,
    pub error: Option<String>,
    /// Unix seconds.
    pub added_at: i64,
}

/// Ask the daemon to look for downloadable files on a page (and, with `depth`, the pages it links to).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct GrabRequest {
    pub url: String,
    /// 0 = only this page, 1 = also the pages it links to, ... (the daemon caps it at 5).
    #[serde(default)]
    pub depth: u32,
    /// Only follow pages on the same host.
    #[serde(default = "yes")]
    pub same_host: bool,
    /// Lowercase extensions without the dot; empty = every link that looks like a file.
    #[serde(default)]
    pub extensions: Vec<String>,
    /// Page limit (the daemon caps it at 300).
    #[serde(default)]
    pub max_pages: u32,
    /// Only links matching one of these patterns (`*` = anything; plain text = contained anywhere). Empty = all.
    #[serde(default)]
    pub include: Vec<String>,
    /// Skip links matching one of these patterns.
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct GrabLink {
    pub url: String,
    pub name: String,
    pub ext: String,
    /// The page it was found on.
    pub from: String,
}

/// One thing that can be downloaded from a video page (a resolution, an audio track, subtitles).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct FormatOption {
    /// What to put in `AddRequest::ytdlp`.
    pub id: String,
    /// `video`, `audio` or `subtitle`.
    pub kind: String,
    /// Text for a menu row, for example `MP4 file, quality 1080p HD`.
    pub label: String,
    pub ext: String,
    pub height: Option<u32>,
    /// Expected download size in bytes (an estimate), when known.
    pub size: Option<u64>,
    /// File name to save under (use it as `AddRequest::filename`).
    pub filename: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct FormatList {
    pub title: String,
    /// Length in seconds, when known.
    pub duration: Option<f64>,
    pub options: Vec<FormatOption>,
}

/// A yt-dlp format choice (`FormatOption::id`) is handed to the program as one argument; accept only the characters
/// format selectors and our own `x:` / `sub:` forms use, and nothing that looks like an option.
pub fn valid_ytdlp_selector(s: &str) -> bool {
    !s.is_empty() && s.len() <= 200 && s.chars().all(|c| c.is_ascii_alphanumeric() || "+-_.[]=/,<>:!*()".contains(c)) && !s.starts_with('-')
}

/// Daemon-wide settings, editable from the Options dialog and saved by the daemon.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Settings {
    /// Connections per download, 1..=32.
    pub connections: u32,
    /// Per-download speed limit in KiB/s; 0 = unlimited.
    pub speed_limit_kib: u64,
    /// Proxy URL (`http://`, `https://`, `socks5://`, `socks5h://`) for all downloads; none = use the environment.
    pub proxy: Option<String>,
    /// Parent of the per-category download folders (absolute path).
    pub download_root: String,
    /// Rate cap for all downloads together in KiB/s; 0 = unlimited.
    #[serde(default)]
    pub global_limit_kib: u64,
    /// Own folder for a category (`Video`, `Documents`, ...), instead of `<download_root>/<category>`.
    #[serde(default)]
    pub category_dirs: std::collections::BTreeMap<String, String>,
    /// How many times a download that failed on a network problem is retried by itself (0..=20).
    #[serde(default = "default_auto_retries")]
    pub auto_retries: u32,
    /// Shell command run after each finished download (for example a virus scan). The file is in `$ZDM_FILE`.
    #[serde(default)]
    pub post_command: Option<String>,
    /// When the file name is already taken: `rename` (name (1).ext), `overwrite` or `skip` (the download fails).
    #[serde(default = "default_on_exists")]
    pub on_exists: String,
    /// Replacement file types (extensions without the dot) of a category, instead of the built-in list.
    #[serde(default)]
    pub category_exts: std::collections::BTreeMap<String, Vec<String>>,
}

fn default_on_exists() -> String {
    "rename".into()
}

/// Built-in file types of each category; anything else is "Other".
pub const CATEGORY_TABLE: &[(&str, &[&str])] = &[
    ("Compressed", &["zip", "7z", "rar", "gz", "bz2", "xz", "zst", "tar", "tgz", "z", "cab", "lzh", "arj"]),
    ("Documents", &["pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp", "txt", "epub", "rtf", "csv"]),
    ("Music", &["mp3", "flac", "aac", "ogg", "opus", "wav", "wma", "m4a", "mpa"]),
    ("Video", &["mp4", "mkv", "avi", "mov", "wmv", "webm", "flv", "m4v", "mpg", "mpeg", "3gp", "ogv", "ts"]),
    ("Programs", &["exe", "msi", "deb", "rpm", "appimage", "apk", "iso", "img", "bin", "sh", "run", "flatpakref", "snap"]),
    ("Images", &["jpg", "jpeg", "png", "gif", "bmp", "webp", "svg", "tif", "tiff", "ico"]),
];
pub const OTHER_CATEGORY: &str = "Other";

fn default_auto_retries() -> u32 {
    3
}

/// What to do once every running and queued download is done (one shot, armed by the user).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AfterAll {
    #[default]
    Nothing,
    Sleep,
    Shutdown,
    Command { command: String },
}

impl AfterAll {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            AfterAll::Command { command } if command.trim().is_empty() || command.len() > 1000 || command.contains('\0') => Err("the command must be 1 to 1000 characters".into()),
            _ => Ok(()),
        }
    }
}

impl Settings {
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=32).contains(&self.connections) {
            return Err("connections must be between 1 and 32".into());
        }
        if let Some(p) = &self.proxy {
            let ok = ["http://", "https://", "socks5://", "socks5h://"].iter().any(|s| p.starts_with(s));
            if !ok {
                return Err("proxy must start with http://, https://, socks5:// or socks5h://".into());
            }
        }
        if !std::path::Path::new(&self.download_root).is_absolute() {
            return Err("download folder must be an absolute path".into());
        }
        if self.auto_retries > 20 {
            return Err("automatic retries must be between 0 and 20".into());
        }
        for (cat, dir) in &self.category_dirs {
            if cat.trim().is_empty() || !std::path::Path::new(dir).is_absolute() {
                return Err(format!("the folder for category \"{cat}\" must be an absolute path"));
            }
        }
        if !["rename", "overwrite", "skip"].contains(&self.on_exists.as_str()) {
            return Err("on_exists must be rename, overwrite or skip".into());
        }
        for (cat, exts) in &self.category_exts {
            if cat.trim().is_empty() || exts.iter().any(|e| e.is_empty() || e.len() > 12 || !e.chars().all(|c| c.is_ascii_alphanumeric())) {
                return Err(format!("file types of category \"{cat}\" must be plain extensions such as zip or mp3"));
            }
        }
        if let Some(c) = &self.post_command {
            if c.trim().is_empty() || c.len() > 1000 || c.contains('\0') {
                return Err("the after-download command must be 1 to 1000 characters".into());
            }
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Add(AddRequest),
    List,
    Get { id: i64 },
    Pause { id: i64 },
    Resume { id: i64 },
    /// Remove a task; optionally delete the downloaded and partial files.
    Remove { id: i64, #[serde(default)] delete_files: bool },
    QueueList,
    /// Create or update a queue.
    QueueSet(QueueInfo),
    QueueRemove { name: String },
    /// Move a task that is not running to another queue.
    MoveTask { id: i64, queue: String },
    /// Start this queued task before the others of its queue.
    MoveToTop { id: i64 },
    /// Give a task that is not running a new address for the same file (keeps what is already downloaded).
    SetUrl { id: i64, url: String },
    GetSettings,
    SetSettings(Settings),
    GetAfterAll,
    SetAfterAll(AfterAll),
    /// Crawl for files (can take a while; answered with `Links`).
    Grab(GrabRequest),
    /// What can be downloaded from a video page (asks yt-dlp; can take ten seconds; answered with `Formats`).
    Formats { url: String },
    Ping,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "res", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Pong { version: String },
    Task(TaskInfo),
    Tasks { tasks: Vec<TaskInfo> },
    Queues { queues: Vec<QueueInfo> },
    Settings(Settings),
    AfterAll(AfterAll),
    Links { links: Vec<GrabLink> },
    Formats(FormatList),
    Error { message: String },
}

/// Directory holding the socket / connection info of a running daemon.
pub fn runtime_dir() -> PathBuf {
    dirs::runtime_dir().or_else(dirs::data_local_dir).unwrap_or_else(std::env::temp_dir).join("zdm")
}

pub fn socket_path() -> PathBuf {
    runtime_dir().join("zdm.sock")
}

/// Windows development builds: where the daemon records its loopback address and token.
pub fn daemon_info_path() -> PathBuf {
    runtime_dir().join("daemon.json")
}

#[derive(Serialize, Deserialize, Debug)]
pub struct DaemonInfo {
    pub addr: String,
    pub token: String,
}

pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

pub async fn connect() -> Result<Box<dyn Io>> {
    #[cfg(unix)]
    {
        let p = socket_path();
        let s = tokio::net::UnixStream::connect(&p)
            .await
            .with_context(|| format!("cannot reach zdmd at {} (is it running?)", p.display()))?;
        Ok(Box::new(s))
    }
    #[cfg(not(unix))]
    {
        let p = daemon_info_path();
        let info: DaemonInfo = serde_json::from_slice(&std::fs::read(&p).with_context(|| format!("cannot read {} (is zdmd running?)", p.display()))?)?;
        let mut s = tokio::net::TcpStream::connect(&info.addr).await.with_context(|| format!("cannot reach zdmd at {}", info.addr))?;
        s.write_all(format!("{}\n", info.token).as_bytes()).await?;
        Ok(Box::new(s))
    }
}

/// Send one request and wait for its response on an existing connection.
pub async fn roundtrip<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut BufReader<S>, req: &Request) -> Result<Response> {
    let mut line = serde_json::to_vec(req)?;
    line.push(b'\n');
    stream.write_all(&line).await?;
    let mut resp = String::new();
    if stream.read_line(&mut resp).await? == 0 {
        return Err(anyhow!("daemon closed the connection"));
    }
    Ok(serde_json::from_str(&resp)?)
}

/// Connect, send one request, return the response (`Response::Error` becomes an `Err`).
pub async fn call(req: &Request) -> Result<Response> {
    let mut s = BufReader::new(connect().await?);
    match roundtrip(&mut s, req).await? {
        Response::Error { message } => Err(anyhow!(message)),
        r => Ok(r),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_same_day() {
        let w = Window { start_min: 9 * 60, end_min: 17 * 60, days: 0b0011111 }; // Mon-Fri
        assert!(w.active_at(9 * 60, 0));
        assert!(!w.active_at(17 * 60, 0));
        assert!(!w.active_at(10 * 60, 5)); // Saturday
    }

    #[test]
    fn window_overnight_belongs_to_start_day() {
        let w = Window { start_min: 23 * 60, end_min: 6 * 60, days: 0b0000001 }; // Monday night
        assert!(w.active_at(23 * 60 + 30, 0)); // Mon 23:30
        assert!(w.active_at(5 * 60, 1)); // Tue 05:00 (still Monday's window)
        assert!(!w.active_at(5 * 60, 0)); // Mon 05:00 belongs to Sunday's window
        assert!(!w.active_at(7 * 60, 1));
    }

    #[test]
    fn window_all_day() {
        let w = Window { start_min: 0, end_min: 0, days: 0x7f };
        assert!(w.active_at(1234, 3));
    }

    #[test]
    fn selector_rule() {
        assert!(valid_ytdlp_selector("299+140") && valid_ytdlp_selector("bestvideo[height<=720]+bestaudio/best") && valid_ytdlp_selector("sub-auto:en"));
        assert!(!valid_ytdlp_selector("") && !valid_ytdlp_selector("--exec") && !valid_ytdlp_selector("a b") && !valid_ytdlp_selector("a;b") && !valid_ytdlp_selector(&"x".repeat(201)));
    }

    #[test]
    fn grab_request_defaults() {
        let r: Request = serde_json::from_str(r#"{"cmd":"grab","url":"http://x/"}"#).unwrap();
        match r {
            Request::Grab(g) => assert!(g.same_host && g.depth == 0 && g.extensions.is_empty() && g.max_pages == 0),
            _ => panic!(),
        }
    }

    #[test]
    fn settings_validation() {
        let ok = Settings { connections: 8, speed_limit_kib: 0, proxy: None, download_root: "/tmp/x".into(), global_limit_kib: 0, category_dirs: Default::default(), auto_retries: 3, post_command: None, on_exists: "rename".into(), category_exts: Default::default() };
        assert!(ok.validate().is_ok());
        assert!(Settings { connections: 0, ..ok.clone() }.validate().is_err());
        assert!(Settings { connections: 33, ..ok.clone() }.validate().is_err());
        assert!(Settings { proxy: Some("ftp://p".into()), ..ok.clone() }.validate().is_err());
        assert!(Settings { proxy: Some("socks5://127.0.0.1:1080".into()), ..ok.clone() }.validate().is_ok());
        assert!(Settings { auto_retries: 21, ..ok.clone() }.validate().is_err());
        assert!(Settings { on_exists: "ask".into(), ..ok.clone() }.validate().is_err());
        assert!(Settings { on_exists: "overwrite".into(), ..ok.clone() }.validate().is_ok());
        let mut exts = std::collections::BTreeMap::new();
        exts.insert("Video".to_string(), vec!["mp4".to_string(), "my file".to_string()]);
        assert!(Settings { category_exts: exts, ..ok.clone() }.validate().is_err());
        assert!(Settings { post_command: Some("  ".into()), ..ok.clone() }.validate().is_err());
        assert!(Settings { post_command: Some("clamscan \"$ZDM_FILE\"".into()), ..ok.clone() }.validate().is_ok());
        let mut dirs = std::collections::BTreeMap::new();
        dirs.insert("Video".to_string(), "movies".to_string());
        assert!(Settings { category_dirs: dirs, ..ok.clone() }.validate().is_err());
        assert!(Settings { download_root: "relative".into(), ..ok }.validate().is_err());
    }

    #[test]
    fn old_settings_files_still_load_and_after_all_is_validated() {
        let s: Settings = serde_json::from_str(r#"{"connections":4,"speed_limit_kib":0,"proxy":null,"download_root":"/d"}"#).unwrap();
        assert_eq!((s.global_limit_kib, s.auto_retries, s.post_command), (0, 3, None));
        assert_eq!(s.on_exists, "rename");
        assert!(AfterAll::Command { command: String::new() }.validate().is_err());
        assert!(AfterAll::Shutdown.validate().is_ok());
        let j = serde_json::to_string(&AfterAll::Command { command: "x".into() }).unwrap();
        assert_eq!(j, r#"{"kind":"command","command":"x"}"#);
    }

    #[test]
    fn request_json_shape() {
        let r: Request = serde_json::from_str(r#"{"cmd":"add","url":"http://x/y"}"#).unwrap();
        match r {
            Request::Add(a) => assert!(a.start && a.headers.is_empty()),
            _ => panic!(),
        }
        assert!(serde_json::from_str::<Request>(r#"{"cmd":"remove","id":3}"#).is_ok());
        assert!(serde_json::from_str::<Request>(r#"{"cmd":"list"}"#).is_ok());
    }
}
