//! Minimal in-process HTTP/1.1 server for tests. Content byte at offset i is `(i % 251) as u8`.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Clone, Default)]
pub struct Config {
    pub size: u64,
    pub ranges: bool,
    /// Answer this many non-probe ranged requests with 503 before behaving.
    pub fail_first_ranged: usize,
    /// Required `Authorization` header value, if any.
    pub auth: Option<String>,
    /// ETag is "v1" for the first request and "v2" afterwards (file replaced mid-download).
    pub flip_etag: bool,
    pub disposition: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Req {
    pub range: Option<String>,
    pub status: u16,
}

pub struct Server {
    pub addr: SocketAddr,
    pub log: Arc<Mutex<Vec<Req>>>,
}

impl Server {
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    pub fn requests(&self) -> Vec<Req> {
        self.log.lock().unwrap().clone()
    }

    /// Start offsets of all non-probe ranged requests.
    pub fn range_starts(&self) -> Vec<u64> {
        self.requests()
            .iter()
            .filter_map(|r| r.range.as_deref())
            .filter(|r| *r != "bytes=0-0")
            .filter_map(|r| r.strip_prefix("bytes=")?.split('-').next()?.parse().ok())
            .collect()
    }
}

pub fn byte(i: u64) -> u8 {
    (i % 251) as u8
}

pub fn expected(size: u64) -> Vec<u8> {
    (0..size).map(byte).collect()
}

pub async fn start(cfg: Config) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let counters = Arc::new((AtomicUsize::new(0), AtomicUsize::new(0))); // (all requests, ranged non-probe)
    let (l2, c2) = (log.clone(), counters);
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else { return };
            let (cfg, log, c) = (cfg.clone(), l2.clone(), c2.clone());
            tokio::spawn(async move {
                let _ = handle(sock, cfg, log, c).await;
            });
        }
    });
    Server { addr, log }
}

async fn handle(mut s: TcpStream, cfg: Config, log: Arc<Mutex<Vec<Req>>>, c: Arc<(AtomicUsize, AtomicUsize)>) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 2048];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = s.read(&mut tmp).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let head = String::from_utf8_lossy(&buf).to_string();
    let header = |name: &str| {
        head.lines().skip(1).find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
        })
    };
    let req_index = c.0.fetch_add(1, Ordering::SeqCst);
    let range = header("range");
    let etag = if cfg.flip_etag && req_index > 0 { "\"v2\"" } else { "\"v1\"" };

    let respond_empty = |status: u16, reason: &'static str| {
        format!("HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
    };

    if let Some(expected_auth) = &cfg.auth {
        if header("authorization").as_deref() != Some(expected_auth.as_str()) {
            log.lock().unwrap().push(Req { range, status: 401 });
            return s.write_all(respond_empty(401, "Unauthorized").as_bytes()).await;
        }
    }
    let is_probe = range.as_deref() == Some("bytes=0-0");
    if range.is_some() && !is_probe && c.1.fetch_add(1, Ordering::SeqCst) < cfg.fail_first_ranged {
        log.lock().unwrap().push(Req { range, status: 503 });
        return s.write_all(respond_empty(503, "Service Unavailable").as_bytes()).await;
    }

    let if_range_ok = header("if-range").is_none_or(|v| v == etag);
    let (mut start, mut end, mut status) = (0u64, cfg.size.saturating_sub(1), 200u16);
    if let (Some(r), true, true) = (&range, cfg.ranges, if_range_ok) {
        if let Some((a, b)) = r.strip_prefix("bytes=").and_then(|x| x.split_once('-')) {
            start = a.parse().unwrap_or(0);
            if let Ok(b) = b.parse::<u64>() {
                end = b.min(cfg.size - 1);
            }
            if start >= cfg.size {
                log.lock().unwrap().push(Req { range, status: 416 });
                return s.write_all(respond_empty(416, "Range Not Satisfiable").as_bytes()).await;
            }
            status = 206;
        }
    }
    log.lock().unwrap().push(Req { range, status });
    let len = end - start + 1;
    let mut h = format!(
        "HTTP/1.1 {} X\r\nContent-Length: {len}\r\nETag: {etag}\r\nConnection: close\r\n",
        status
    );
    if cfg.ranges {
        h.push_str("Accept-Ranges: bytes\r\n");
    }
    if status == 206 {
        h.push_str(&format!("Content-Range: bytes {start}-{end}/{}\r\n", cfg.size));
    }
    if let Some(d) = &cfg.disposition {
        h.push_str(&format!("Content-Disposition: {d}\r\n"));
    }
    h.push_str("\r\n");
    s.write_all(h.as_bytes()).await?;
    let mut off = start;
    while off <= end {
        let n = (end - off + 1).min(64 * 1024);
        let chunk: Vec<u8> = (off..off + n).map(byte).collect();
        s.write_all(&chunk).await?;
        off += n;
    }
    s.shutdown().await
}

/// A tiny static site for crawler/HLS tests: `routes` maps a path to (content type, body). Every GET gets the
/// whole body (Range is ignored); unknown paths get 404. `log` records the requested paths.
pub struct Site {
    pub addr: SocketAddr,
    pub hits: Arc<Mutex<Vec<String>>>,
}

impl Site {
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }
    pub fn hits(&self) -> Vec<String> {
        self.hits.lock().unwrap().clone()
    }
}

pub type Routes = std::collections::HashMap<String, (String, Vec<u8>)>;

pub async fn serve_site(routes: Routes) -> Site {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(Mutex::new(Vec::new()));
    let (routes, h2) = (Arc::new(routes), hits.clone());
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else { return };
            let (routes, hits) = (routes.clone(), h2.clone());
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 2048];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut tmp).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&buf).to_string();
                let path = head.split_whitespace().nth(1).unwrap_or("/").split('?').next().unwrap_or("/").to_string();
                hits.lock().unwrap().push(path.clone());
                let resp: Vec<u8> = match routes.get(&path) {
                    Some((ct, body)) => {
                        let mut r = format!("HTTP/1.1 200 OK\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
                        r.extend_from_slice(body);
                        r
                    }
                    None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                };
                let _ = s.write_all(&resp).await;
                let _ = s.shutdown().await;
            });
        }
    });
    Site { addr, hits }
}
