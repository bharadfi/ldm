use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use reqwest::header::{IF_RANGE, RANGE};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;

use crate::probe::{build_client, probe, Probe};

#[derive(Clone, Debug)]
pub struct Options {
    /// Maximum parallel connections.
    pub connections: usize,
    /// Never split a remaining range smaller than twice this many bytes.
    pub min_split: u64,
    pub user_agent: String,
    pub referer: Option<String>,
    pub headers: Vec<(String, String)>,
    /// Retries per segment after a network error.
    pub retries: u32,
    /// Proxy URL for all schemes (otherwise the standard proxy environment variables apply).
    pub proxy: Option<String>,
    /// HTTP Basic credentials. Marked sensitive, so reqwest does not forward them across hosts on redirect.
    pub auth: Option<(String, String)>,
    /// Total download rate cap in bytes per second, shared by all connections.
    pub limit_bps: Option<u64>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            connections: 8,
            min_split: 1 << 20,
            user_agent: concat!("zdm/", env!("CARGO_PKG_VERSION")).to_string(),
            referer: None,
            headers: Vec::new(),
            retries: 5,
            proxy: None,
            auth: None,
            limit_bps: None,
        }
    }
}

/// The file on the server changed (or the server broke the Range contract) after we planned or resumed.
#[derive(Debug)]
struct ResumeInvalid(String);

impl std::fmt::Display for ResumeInvalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ResumeInvalid {}

/// Paces all connections to a shared byte rate.
struct Limiter {
    bps: u64,
    next: Mutex<std::time::Instant>,
}

impl Limiter {
    fn new(bps: u64) -> Self {
        Self { bps: bps.max(1), next: Mutex::new(std::time::Instant::now()) }
    }

    async fn wait(&self, n: usize) {
        let delay = {
            let mut next = self.next.lock().unwrap();
            let now = std::time::Instant::now();
            let start = (*next).max(now);
            *next = start + Duration::from_secs_f64(n as f64 / self.bps as f64);
            start.saturating_duration_since(now)
        };
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
    }
}

type Lim = Option<Arc<Limiter>>;

/// Half-open byte range `[start, end)` with a write cursor `pos`.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct Seg {
    start: u64,
    end: u64,
    pos: u64,
}

impl Seg {
    fn remaining(&self) -> u64 {
        self.end.saturating_sub(self.pos)
    }
    fn done_bytes(&self) -> u64 {
        self.pos.min(self.end).saturating_sub(self.start)
    }
}

/// Resume record stored next to the partial file.
#[derive(Serialize, Deserialize)]
struct Sidecar {
    url: String,
    size: u64,
    etag: Option<String>,
    last_modified: Option<String>,
    segs: Vec<Seg>,
}

struct Shared {
    segs: Vec<Seg>,
    claimed: Vec<bool>,
}

impl Shared {
    fn done_bytes(&self) -> u64 {
        self.segs.iter().map(Seg::done_bytes).sum()
    }

    /// Give a worker something to do: an unclaimed segment, else half of the
    /// largest segment that is still big enough to split.
    fn next_job(&mut self, min_split: u64) -> Option<usize> {
        if let Some(i) = (0..self.segs.len()).find(|&i| !self.claimed[i] && self.segs[i].remaining() > 0) {
            self.claimed[i] = true;
            return Some(i);
        }
        let (victim, rem) = self
            .segs
            .iter()
            .enumerate()
            .filter(|(i, s)| self.claimed[*i] && s.remaining() >= 2 * min_split)
            .map(|(i, s)| (i, s.remaining()))
            .max_by_key(|&(_, r)| r)?;
        let mid = self.segs[victim].pos + rem / 2;
        let end = self.segs[victim].end;
        self.segs[victim].end = mid;
        self.segs.push(Seg { start: mid, end, pos: mid });
        self.claimed.push(true);
        Some(self.segs.len() - 1)
    }
}

fn sibling(dest: &Path, suffix: &str) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

#[cfg(unix)]
fn write_at(f: &File, buf: &[u8], off: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(f, buf, off)
}

#[cfg(windows)]
fn write_at(f: &File, mut buf: &[u8], mut off: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = f.seek_write(buf, off)?;
        if n == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        buf = &buf[n..];
        off += n as u64;
    }
    Ok(())
}

/// Probe `url` and download it to `dest`. `on_progress(done, total)` is called about once per second
/// (`total` is 0 when unknown).
pub async fn download(url: &str, dest: &Path, opts: &Options, on_progress: impl Fn(u64, u64) + Send + Sync + 'static) -> Result<()> {
    let client = build_client(opts)?;
    let p = probe(&client, url).await?;
    download_probed(&client, url, &p, dest, opts, on_progress).await
}

/// Download with an existing probe result (lets callers pick the file name first).
pub async fn download_probed(
    client: &Client,
    url: &str,
    p: &Probe,
    dest: &Path,
    opts: &Options,
    on_progress: impl Fn(u64, u64) + Send + Sync + 'static,
) -> Result<()> {
    let part = sibling(dest, ".part");
    let side = sibling(dest, ".part.zdm");
    let lim: Lim = opts.limit_bps.map(|b| Arc::new(Limiter::new(b)));
    let on_progress = Arc::new(on_progress);
    match (p.size, p.ranges) {
        (Some(size), true) => {
            let cb = on_progress.clone();
            let mut r = ranged(client, url, p, size, &part, &side, opts, &lim, move |d, t| cb(d, t)).await;
            // The file changed under us: discard partial data, look again, and start over once.
            if matches!(&r, Err(e) if e.downcast_ref::<ResumeInvalid>().is_some()) {
                let _ = std::fs::remove_file(&part);
                let _ = std::fs::remove_file(&side);
                let p2 = probe(client, url).await?;
                r = match (p2.size, p2.ranges) {
                    (Some(size2), true) => {
                        let cb = on_progress.clone();
                        ranged(client, url, &p2, size2, &part, &side, opts, &lim, move |d, t| cb(d, t)).await
                    }
                    _ => {
                        let cb = on_progress.clone();
                        single(client, url, p2.size, &part, &lim, move |d, t| cb(d, t)).await
                    }
                };
            }
            r?
        }
        _ => {
            let cb = on_progress.clone();
            single(client, url, p.size, &part, &lim, move |d, t| cb(d, t)).await?
        }
    }
    std::fs::rename(&part, dest).with_context(|| format!("cannot move {} to {}", part.display(), dest.display()))?;
    let _ = std::fs::remove_file(&side);
    Ok(())
}

/// Fallback when ranges are unavailable: one stream, restart from zero every time.
async fn single(client: &Client, url: &str, size: Option<u64>, part: &Path, lim: &Lim, on_progress: impl Fn(u64, u64)) -> Result<()> {
    let resp = client.get(url).send().await?.error_for_status()?;
    let file = File::create(part)?;
    let mut off = 0u64;
    let mut last = std::time::Instant::now();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if let Some(l) = lim {
            l.wait(chunk.len()).await;
        }
        write_at(&file, &chunk, off)?;
        off += chunk.len() as u64;
        if last.elapsed() >= Duration::from_secs(1) {
            on_progress(off, size.unwrap_or(0));
            last = std::time::Instant::now();
        }
    }
    if let Some(s) = size {
        if off != s {
            bail!("short download: {off} of {s} bytes");
        }
    }
    on_progress(off, size.unwrap_or(0));
    Ok(())
}

fn plan(size: u64, n: usize, min_split: u64) -> Vec<Seg> {
    let n = ((size / min_split.max(1)).max(1) as usize).min(n.max(1)) as u64;
    let step = size / n;
    (0..n)
        .map(|i| {
            let start = i * step;
            let end = if i + 1 == n { size } else { start + step };
            Seg { start, end, pos: start }
        })
        .collect()
}

fn load_sidecar(side: &Path, part: &Path, url: &str, p: &Probe, size: u64) -> Option<Vec<Seg>> {
    let sc: Sidecar = serde_json::from_slice(&std::fs::read(side).ok()?).ok()?;
    let part_len = std::fs::metadata(part).ok()?.len();
    let ok = sc.url == url && sc.size == size && part_len == size && sc.etag == p.etag && sc.last_modified == p.last_modified
        && sc.segs.iter().all(|s| s.start <= s.pos && s.pos <= s.end && s.end <= size);
    ok.then_some(sc.segs)
}

fn save_sidecar(side: &Path, url: &str, p: &Probe, size: u64, shared: &Mutex<Shared>) -> Result<()> {
    let segs = shared.lock().unwrap().segs.iter().map(|s| Seg { pos: s.pos.min(s.end), ..s.clone() }).collect();
    let sc = Sidecar { url: url.to_string(), size, etag: p.etag.clone(), last_modified: p.last_modified.clone(), segs };
    let tmp = sibling(side, ".tmp");
    std::fs::write(&tmp, serde_json::to_vec(&sc)?)?;
    std::fs::rename(tmp, side)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn ranged(
    client: &Client,
    url: &str,
    p: &Probe,
    size: u64,
    part: &Path,
    side: &Path,
    opts: &Options,
    lim: &Lim,
    on_progress: impl Fn(u64, u64) + Send + Sync + 'static,
) -> Result<()> {
    let segs = load_sidecar(side, part, url, p, size).unwrap_or_else(|| plan(size, opts.connections, opts.min_split));
    let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(part)?;
    file.set_len(size)?;
    let file = Arc::new(file);
    let claimed = vec![false; segs.len()];
    let shared = Arc::new(Mutex::new(Shared { segs, claimed }));
    // Only send If-Range when the server gave a strong validator.
    let validator = p.etag.clone().filter(|e| !e.starts_with("W/"));

    let mut set = JoinSet::new();
    for _ in 0..opts.connections.max(1) {
        let (client, url, file, shared, validator, opts, lim) =
            (client.clone(), url.to_string(), file.clone(), shared.clone(), validator.clone(), opts.clone(), lim.clone());
        set.spawn(async move {
            loop {
                let job = shared.lock().unwrap().next_job(opts.min_split);
                match job {
                    Some(i) => run_segment(&client, &url, &file, &shared, i, validator.as_deref(), opts.retries, &lim).await?,
                    None => return Ok::<(), anyhow::Error>(()),
                }
            }
        });
    }

    let ticker = {
        let (shared, side, url, p) = (shared.clone(), side.to_path_buf(), url.to_string(), p.clone());
        let on_progress = Arc::new(on_progress);
        let cb = on_progress.clone();
        let handle = tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tick.tick().await;
                let done = shared.lock().unwrap().done_bytes();
                let _ = save_sidecar(&side, &url, &p, size, &shared);
                cb(done, size);
            }
        });
        (handle, on_progress)
    };

    let mut result = Ok(());
    while let Some(r) = set.join_next().await {
        if let Err(e) = r.map_err(|e| anyhow!(e)).and_then(|r| r) {
            result = Err(e);
            set.abort_all();
            break;
        }
    }
    ticker.0.abort();
    let done = shared.lock().unwrap().done_bytes();
    (ticker.1)(done, size);
    if result.is_err() {
        let _ = save_sidecar(side, url, p, size, &shared); // keep resume state
        return result;
    }
    if done != size {
        let _ = save_sidecar(side, url, p, size, &shared);
        bail!("incomplete download: {done} of {size} bytes");
    }
    file.sync_all()?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_segment(
    client: &Client,
    url: &str,
    file: &File,
    shared: &Mutex<Shared>,
    idx: usize,
    validator: Option<&str>,
    retries: u32,
    lim: &Lim,
) -> Result<()> {
    let mut attempt = 0;
    loop {
        let (pos, end) = {
            let s = &shared.lock().unwrap().segs[idx];
            (s.pos, s.end)
        };
        if pos >= end {
            return Ok(());
        }
        match fetch_range(client, url, file, shared, idx, pos, end, validator, lim).await {
            Ok(()) => {
                let s = &shared.lock().unwrap().segs[idx];
                if s.pos >= s.end {
                    return Ok(());
                }
                // Stream ended early: treat as a retryable error.
                if attempt >= retries {
                    bail!("segment {idx} ended early at {} of {}", s.pos, s.end);
                }
            }
            Err(FetchError::Fatal(e)) => return Err(e),
            Err(FetchError::Retry(e)) => {
                if attempt >= retries {
                    return Err(e.context(format!("segment {idx} failed after {retries} retries")));
                }
            }
        }
        attempt += 1;
        tokio::time::sleep(Duration::from_millis(250 * (1 << attempt.min(5)))).await;
    }
}

enum FetchError {
    Retry(anyhow::Error),
    Fatal(anyhow::Error),
}

#[allow(clippy::too_many_arguments)]
async fn fetch_range(
    client: &Client,
    url: &str,
    file: &File,
    shared: &Mutex<Shared>,
    idx: usize,
    pos: u64,
    end: u64,
    validator: Option<&str>,
    lim: &Lim,
) -> std::result::Result<(), FetchError> {
    let mut req = client.get(url).header(RANGE, format!("bytes={}-{}", pos, end - 1));
    if let Some(v) = validator {
        req = req.header(IF_RANGE, v);
    }
    let resp = req.send().await.map_err(|e| FetchError::Retry(e.into()))?;
    match resp.status() {
        StatusCode::PARTIAL_CONTENT => {}
        // With If-Range, a 200 means the resource changed; without it the server broke the Range contract.
        StatusCode::OK => {
            return Err(FetchError::Fatal(anyhow::Error::new(ResumeInvalid(
                "server ignored range or file changed (got 200 for a ranged request)".into(),
            ))))
        }
        StatusCode::RANGE_NOT_SATISFIABLE => {
            return Err(FetchError::Fatal(anyhow::Error::new(ResumeInvalid("range not satisfiable: file changed or truncated".into()))))
        }
        s if s.is_server_error() || s == StatusCode::TOO_MANY_REQUESTS => return Err(FetchError::Retry(anyhow!("server returned {s}"))),
        s => return Err(FetchError::Fatal(anyhow!("server returned {s}"))),
    }
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| FetchError::Retry(e.into()))?;
        if let Some(l) = lim {
            l.wait(chunk.len()).await;
        }
        let mut data: &[u8] = &chunk;
        while !data.is_empty() {
            // Another worker may have taken the tail of this segment; write only what is still ours.
            let (at, take) = {
                let s = &shared.lock().unwrap().segs[idx];
                if s.pos >= s.end {
                    return Ok(());
                }
                (s.pos, (data.len() as u64).min(s.end - s.pos) as usize)
            };
            write_at(file, &data[..take], at).map_err(|e| FetchError::Fatal(e.into()))?;
            shared.lock().unwrap().segs[idx].pos += take as u64;
            data = &data[take..];
        }
    }
    Ok(())
}

#[cfg(test)]
mod http_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_covers_file_without_gaps() {
        let segs = plan(10_000_001, 8, 1 << 20);
        assert_eq!(segs.first().unwrap().start, 0);
        assert_eq!(segs.last().unwrap().end, 10_000_001);
        for w in segs.windows(2) {
            assert_eq!(w[0].end, w[1].start);
        }
        assert!(segs.len() <= 8);
    }

    #[test]
    fn plan_small_file_single_segment() {
        assert_eq!(plan(1000, 8, 1 << 20).len(), 1);
    }

    #[test]
    fn stealing_splits_largest_and_stays_contiguous() {
        let mut sh = Shared { segs: vec![Seg { start: 0, end: 100 << 20, pos: 0 }], claimed: vec![false] };
        assert_eq!(sh.next_job(1 << 20), Some(0));
        assert_eq!(sh.next_job(1 << 20), Some(1));
        assert_eq!(sh.segs[0].end, sh.segs[1].start);
        assert_eq!(sh.segs[1].end, 100 << 20);
        // Too small to split further.
        let mut tiny = Shared { segs: vec![Seg { start: 0, end: 100, pos: 0 }], claimed: vec![true] };
        assert_eq!(tiny.next_job(1 << 20), None);
    }
}
