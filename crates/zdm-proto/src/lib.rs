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
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct GrabLink {
    pub url: String,
    pub name: String,
    pub ext: String,
    /// The page it was found on.
    pub from: String,
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
    GetSettings,
    SetSettings(Settings),
    /// Crawl for files (can take a while; answered with `Links`).
    Grab(GrabRequest),
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
    Links { links: Vec<GrabLink> },
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
    fn grab_request_defaults() {
        let r: Request = serde_json::from_str(r#"{"cmd":"grab","url":"http://x/"}"#).unwrap();
        match r {
            Request::Grab(g) => assert!(g.same_host && g.depth == 0 && g.extensions.is_empty() && g.max_pages == 0),
            _ => panic!(),
        }
    }

    #[test]
    fn settings_validation() {
        let ok = Settings { connections: 8, speed_limit_kib: 0, proxy: None, download_root: "/tmp/x".into() };
        assert!(ok.validate().is_ok());
        assert!(Settings { connections: 0, ..ok.clone() }.validate().is_err());
        assert!(Settings { connections: 33, ..ok.clone() }.validate().is_err());
        assert!(Settings { proxy: Some("ftp://p".into()), ..ok.clone() }.validate().is_err());
        assert!(Settings { proxy: Some("socks5://127.0.0.1:1080".into()), ..ok.clone() }.validate().is_ok());
        assert!(Settings { download_root: "relative".into(), ..ok }.validate().is_err());
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
