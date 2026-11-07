//! HLS (RFC 8216) download: parse master/media playlists, fetch the segments in parallel (resumable),
//! decrypt AES-128, join them and, when ffmpeg is installed, remux to MP4.
//!
//! Not supported (clear errors instead of broken files): live streams (no `#EXT-X-ENDLIST`),
//! SAMPLE-AES / other encryption methods.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use cbc::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use futures_util::stream::{self, StreamExt};
use reqwest::Client;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::Cancelled;

#[derive(Clone, Debug, PartialEq)]
pub struct Variant {
    pub url: Url,
    pub bandwidth: u64,
    pub resolution: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct KeyInfo {
    pub uri: Url,
    pub iv: Option<[u8; 16]>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ByteRange {
    pub start: u64,
    pub len: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    pub url: Url,
    /// Media sequence number (default AES IV).
    pub seq: u64,
    pub key: Option<KeyInfo>,
    pub range: Option<ByteRange>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Media {
    /// fMP4 initialisation section, written before the first segment.
    pub init: Option<Segment>,
    pub segments: Vec<Segment>,
    pub ended: bool,
}

#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum Playlist {
    Master(Vec<Variant>),
    Media(Media),
}

/// `KEY=value,KEY2="quoted, value"` -> pairs (keys upper-cased, quotes removed).
fn attrs(s: &str) -> Vec<(String, String)> {
    let (mut out, mut key, mut val, mut in_key, mut quoted) = (vec![], String::new(), String::new(), true, false);
    for c in s.chars().chain(std::iter::once(',')) {
        match c {
            '=' if in_key => in_key = false,
            '"' => quoted = !quoted,
            ',' if !quoted => {
                if !key.trim().is_empty() {
                    out.push((key.trim().to_ascii_uppercase(), val.clone()));
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

fn attr<'a>(a: &'a [(String, String)], k: &str) -> Option<&'a str> {
    a.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
}

fn parse_iv(s: &str) -> Result<[u8; 16]> {
    let h = s.trim_start_matches("0x").trim_start_matches("0X");
    if h.len() > 32 || h.is_empty() || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("bad IV in playlist: {s}");
    }
    let padded = format!("{h:0>32}");
    let mut iv = [0u8; 16];
    for (i, b) in iv.iter_mut().enumerate() {
        *b = u8::from_str_radix(&padded[i * 2..i * 2 + 2], 16)?;
    }
    Ok(iv)
}

/// `len` or `len@offset`.
fn parse_range(s: &str) -> Result<(u64, Option<u64>)> {
    let (l, o) = match s.split_once('@') {
        Some((l, o)) => (l, Some(o.parse()?)),
        None => (s, None),
    };
    Ok((l.parse()?, o))
}

/// (bandwidth, resolution) of the `#EXT-X-STREAM-INF` line waiting for its URI.
type PendingVariant = (u64, Option<String>);

pub fn parse(text: &str, base: &Url) -> Result<Playlist> {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    if lines.next() != Some("#EXTM3U") {
        bail!("not an HLS playlist (missing #EXTM3U)");
    }
    let (mut variants, mut segments) = (vec![], vec![]);
    let mut pending_variant: Option<PendingVariant> = None;
    let mut pending_range: Option<(u64, Option<u64>)> = None;
    let (mut key, mut init, mut seq, mut ended) = (None::<KeyInfo>, None::<Segment>, 0u64, false);
    let mut next_start: HashMap<String, u64> = HashMap::new();
    for line in lines {
        if let Some(rest) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            let a = attrs(rest);
            pending_variant = Some((attr(&a, "BANDWIDTH").and_then(|v| v.parse().ok()).unwrap_or(0), attr(&a, "RESOLUTION").map(str::to_string)));
        } else if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            seq = rest.trim().parse().unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("#EXT-X-KEY:") {
            let a = attrs(rest);
            match attr(&a, "METHOD") {
                Some("NONE") | None => key = None,
                Some("AES-128") => {
                    let uri = attr(&a, "URI").ok_or_else(|| anyhow!("AES-128 key without URI"))?;
                    key = Some(KeyInfo { uri: base.join(uri)?, iv: attr(&a, "IV").map(parse_iv).transpose()? });
                }
                Some(m) => bail!("unsupported HLS encryption method {m}"),
            }
        } else if let Some(rest) = line.strip_prefix("#EXT-X-MAP:") {
            let a = attrs(rest);
            let uri = attr(&a, "URI").ok_or_else(|| anyhow!("EXT-X-MAP without URI"))?;
            let range = attr(&a, "BYTERANGE").map(parse_range).transpose()?.map(|(len, off)| ByteRange { start: off.unwrap_or(0), len });
            init = Some(Segment { url: base.join(uri)?, seq: 0, key: key.clone(), range });
        } else if let Some(rest) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            pending_range = Some(parse_range(rest.trim())?);
        } else if line == "#EXT-X-ENDLIST" {
            ended = true;
        } else if !line.starts_with('#') {
            let url = base.join(line)?;
            if let Some((bandwidth, resolution)) = pending_variant.take() {
                variants.push(Variant { url, bandwidth, resolution });
            } else {
                let range = pending_range.take().map(|(len, off)| {
                    let start = off.unwrap_or_else(|| next_start.get(url.as_str()).copied().unwrap_or(0));
                    next_start.insert(url.to_string(), start + len);
                    ByteRange { start, len }
                });
                segments.push(Segment { url, seq, key: key.clone(), range });
                seq += 1;
            }
        }
    }
    if !variants.is_empty() {
        return Ok(Playlist::Master(variants));
    }
    Ok(Playlist::Media(Media { init, segments, ended }))
}

/// Index of the variant to download: the requested one, else the highest bandwidth.
pub fn pick_variant(v: &[Variant], want: Option<usize>) -> usize {
    match want {
        Some(i) if i < v.len() => i,
        _ => v.iter().enumerate().max_by_key(|(_, x)| x.bandwidth).map_or(0, |(i, _)| i),
    }
}

/// ffmpeg for remuxing: `ZDM_FFMPEG=none` disables, `ZDM_FFMPEG=/path` overrides, else PATH.
pub fn ffmpeg_path() -> Option<PathBuf> {
    match std::env::var("ZDM_FFMPEG") {
        Ok(v) if v == "none" => return None,
        Ok(v) if !v.is_empty() => return Some(PathBuf::from(v)),
        _ => {}
    }
    std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join("ffmpeg")).find(|f| f.is_file()))
}

pub fn is_hls_url(url: &str) -> bool {
    Url::parse(url).map(|u| u.path().to_ascii_lowercase().ends_with(".m3u8")).unwrap_or(false)
}

#[derive(Clone)]
pub struct HlsOptions {
    pub cancel: CancellationToken,
    pub connections: usize,
    /// Convert to MP4 with ffmpeg when it is available (otherwise the result is a `.ts` file).
    pub remux: bool,
    pub variant: Option<usize>,
}

impl Default for HlsOptions {
    fn default() -> Self {
        HlsOptions { cancel: CancellationToken::new(), connections: 4, remux: true, variant: None }
    }
}

async fn get(client: &Client, url: &Url, range: Option<&ByteRange>, cancel: &CancellationToken) -> Result<Vec<u8>> {
    let mut last = anyhow!("no attempt made");
    for attempt in 0..4u64 {
        if attempt > 0 {
            tokio::select! { _ = cancel.cancelled() => return Err(Cancelled.into()), _ = tokio::time::sleep(Duration::from_millis(300 * attempt)) => {} }
        }
        let mut req = client.get(url.clone());
        if let Some(r) = range {
            req = req.header(reqwest::header::RANGE, format!("bytes={}-{}", r.start, r.start + r.len - 1));
        }
        let res = tokio::select! { _ = cancel.cancelled() => return Err(Cancelled.into()), r = async { req.send().await?.error_for_status()?.bytes().await } => r };
        match res {
            Ok(b) => return Ok(b.to_vec()),
            Err(e) => last = anyhow!("{url}: {e}"),
        }
    }
    Err(last)
}

fn decrypt(data: &[u8], key: &[u8; 16], iv: &[u8; 16]) -> Result<Vec<u8>> {
    cbc::Decryptor::<aes::Aes128>::new(key.into(), iv.into()).decrypt_padded_vec_mut::<Pkcs7>(data).map_err(|_| anyhow!("could not decrypt a segment (wrong key or corrupt data)"))
}

fn seg_path(dir: &Path, i: usize) -> PathBuf {
    dir.join(format!("seg-{i:06}"))
}

/// Download the stream behind `url` into `dest` (a `.ts`, or `.mp4` when `opts.remux` and ffmpeg exist).
/// Finished segments are kept in `<dest>.hls/` so an interrupted download resumes where it stopped.
/// Returns the file actually written (ffmpeg trouble falls back to `.ts`). `on_progress(done, estimated_total)`.
pub async fn download_hls(client: &Client, url: &str, dest: &Path, opts: &HlsOptions, on_progress: impl Fn(u64, u64) + Send + Sync + 'static) -> Result<PathBuf> {
    let mut playlist_url = Url::parse(url)?;
    let media = loop {
        let text = String::from_utf8_lossy(&get(client, &playlist_url, None, &opts.cancel).await?).into_owned();
        match parse(&text, &playlist_url).with_context(|| format!("reading {playlist_url}"))? {
            Playlist::Master(v) => playlist_url = v[pick_variant(&v, opts.variant)].url.clone(),
            Playlist::Media(m) => break m,
        }
    };
    if !media.ended {
        bail!("this is a live stream (no end marker); live streams are not supported");
    }
    if media.segments.is_empty() {
        bail!("the playlist has no segments");
    }
    let mut dir = dest.as_os_str().to_owned();
    dir.push(".hls");
    let dir = PathBuf::from(dir);
    std::fs::create_dir_all(&dir)?;

    let all: Vec<Segment> = media.init.iter().cloned().chain(media.segments.iter().cloned()).collect();
    let total = all.len();
    let done_bytes = Arc::new(AtomicU64::new(0));
    let done_count = Arc::new(AtomicU64::new(0));
    for i in 0..total {
        if let Ok(m) = std::fs::metadata(seg_path(&dir, i)) {
            done_bytes.fetch_add(m.len(), Ordering::SeqCst);
            done_count.fetch_add(1, Ordering::SeqCst);
        }
    }
    let progress = Arc::new(on_progress);
    let report = {
        let (b, c, p) = (done_bytes.clone(), done_count.clone(), progress.clone());
        move || {
            let (bytes, n) = (b.load(Ordering::SeqCst), c.load(Ordering::SeqCst).max(1));
            p(bytes, bytes * total as u64 / n);
        }
    };
    report();

    let keys: Arc<tokio::sync::Mutex<HashMap<String, [u8; 16]>>> = Arc::default();
    let results: Vec<Result<()>> = stream::iter(0..total)
        .map(|i| {
            let (seg, dir, client, cancel, keys) = (all[i].clone(), dir.clone(), client.clone(), opts.cancel.clone(), keys.clone());
            let (b, c, report) = (done_bytes.clone(), done_count.clone(), report.clone());
            async move {
                let path = seg_path(&dir, i);
                if path.exists() {
                    return Ok(());
                }
                if cancel.is_cancelled() {
                    return Err(Cancelled.into());
                }
                let mut data = get(&client, &seg.url, seg.range.as_ref(), &cancel).await?;
                if let Some(k) = &seg.key {
                    let key = {
                        let mut cache = keys.lock().await;
                        match cache.get(k.uri.as_str()) {
                            Some(v) => *v,
                            None => {
                                let raw = get(&client, &k.uri, None, &cancel).await?;
                                let arr: [u8; 16] = raw.as_slice().try_into().map_err(|_| anyhow!("the AES key is {} bytes, expected 16", raw.len()))?;
                                cache.insert(k.uri.to_string(), arr);
                                arr
                            }
                        }
                    };
                    let iv = k.iv.unwrap_or_else(|| (seg.seq as u128).to_be_bytes());
                    data = decrypt(&data, &key, &iv)?;
                }
                let tmp = path.with_extension("tmp");
                std::fs::write(&tmp, &data)?;
                std::fs::rename(&tmp, &path)?;
                b.fetch_add(data.len() as u64, Ordering::SeqCst);
                c.fetch_add(1, Ordering::SeqCst);
                report();
                Ok(())
            }
        })
        .buffer_unordered(opts.connections.clamp(1, 16))
        .collect()
        .await;
    if opts.cancel.is_cancelled() {
        return Err(Cancelled.into());
    }
    for r in results {
        r?;
    }

    // join
    let joined = dir.join("all.ts");
    {
        let (dir, joined) = (dir.clone(), joined.clone());
        tokio::task::spawn_blocking(move || -> Result<()> {
            use std::io::Write;
            let mut out = std::io::BufWriter::new(std::fs::File::create(&joined)?);
            for i in 0..total {
                out.write_all(&std::fs::read(seg_path(&dir, i))?)?;
            }
            out.flush()?;
            Ok(())
        })
        .await??;
    }
    let want_mp4 = dest.extension().is_some_and(|e| e.eq_ignore_ascii_case("mp4"));
    let mut final_path = dest.to_path_buf();
    let mut done = false;
    if want_mp4 && opts.remux {
        if let Some(ff) = ffmpeg_path() {
            let (out, input) = (dest.with_extension("mp4.tmp"), joined.clone());
            let ok = tokio::task::spawn_blocking(move || {
                std::process::Command::new(ff).args(["-y", "-loglevel", "error", "-i"]).arg(&input).args(["-c", "copy", "-f", "mp4"]).arg(&out).status().map(|s| s.success() && out.exists()).unwrap_or(false)
            })
            .await?;
            if ok {
                std::fs::rename(dest.with_extension("mp4.tmp"), dest)?;
                done = true;
            } else {
                let _ = std::fs::remove_file(dest.with_extension("mp4.tmp"));
            }
        }
    }
    if !done {
        // plain transport stream (no ffmpeg, remux off, or ffmpeg could not convert it)
        if want_mp4 {
            final_path = dest.with_extension("ts");
        }
        std::fs::rename(&joined, &final_path)?;
    }
    let _ = std::fs::remove_dir_all(&dir);
    Ok(final_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testserver::{serve_site, Routes};

    fn base() -> Url {
        Url::parse("http://h.test/v/index.m3u8").unwrap()
    }

    #[test]
    fn master_playlist_and_variant_choice() {
        let m = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360\nlow/p.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=2400000,RESOLUTION=1280x720,CODECS=\"avc1.4d401f,mp4a.40.2\"\nhigh/p.m3u8\n";
        let Playlist::Master(v) = parse(m, &base()).unwrap() else { panic!("not master") };
        assert_eq!(v.len(), 2);
        assert_eq!(v[1].url.as_str(), "http://h.test/v/high/p.m3u8");
        assert_eq!(v[1].resolution.as_deref(), Some("1280x720"));
        assert_eq!(pick_variant(&v, None), 1);
        assert_eq!(pick_variant(&v, Some(0)), 0);
        assert_eq!(pick_variant(&v, Some(9)), 1);
    }

    #[test]
    fn media_playlist_with_keys_ranges_and_map() {
        let m = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:7\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXT-X-KEY:METHOD=AES-128,URI=\"k.bin\",IV=0xA1\n#EXTINF:4,\n#EXT-X-BYTERANGE:100@0\na.ts\n#EXTINF:4,\n#EXT-X-BYTERANGE:50\na.ts\n#EXT-X-KEY:METHOD=NONE\n#EXTINF:4,\nb.ts\n#EXT-X-ENDLIST\n";
        let Playlist::Media(md) = parse(m, &base()).unwrap() else { panic!("not media") };
        assert!(md.ended);
        assert_eq!(md.init.as_ref().unwrap().url.as_str(), "http://h.test/v/init.mp4");
        assert_eq!(md.segments.len(), 3);
        assert_eq!(md.segments[0].seq, 7);
        assert_eq!(md.segments[0].range, Some(ByteRange { start: 0, len: 100 }));
        assert_eq!(md.segments[1].range, Some(ByteRange { start: 100, len: 50 }), "an omitted offset continues the previous range");
        let k = md.segments[0].key.as_ref().unwrap();
        assert_eq!(k.uri.as_str(), "http://h.test/v/k.bin");
        assert_eq!(k.iv.unwrap()[15], 0xa1);
        assert!(md.segments[2].key.is_none());
    }

    #[test]
    fn rejects_unsupported_and_garbage() {
        assert!(parse("hello", &base()).is_err());
        assert!(parse("#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"k\"\n#EXTINF:1,\na.ts\n", &base()).is_err());
        assert!(is_hls_url("https://x.test/a/b.M3U8?token=1"));
        assert!(!is_hls_url("https://x.test/a/b.mp4"));
    }

    fn seg_bytes(i: usize) -> Vec<u8> {
        (0..5000).map(|n| ((n + i * 7) % 253) as u8).collect()
    }

    fn playlist(n: usize, extra_head: &str) -> String {
        let mut s = format!("#EXTM3U\n#EXT-X-TARGETDURATION:4\n{extra_head}");
        for i in 0..n {
            s += &format!("#EXTINF:4,\ns{i}.ts\n");
        }
        s + "#EXT-X-ENDLIST\n"
    }

    fn opts() -> HlsOptions {
        HlsOptions { remux: false, connections: 3, ..Default::default() }
    }

    #[tokio::test]
    async fn downloads_and_joins_segments_via_master_playlist() {
        let mut r = Routes::new();
        r.insert("/master.m3u8".into(), ("application/vnd.apple.mpegurl".into(), b"#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=100\nlow.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=900\nhigh.m3u8\n".to_vec()));
        r.insert("/high.m3u8".into(), ("application/vnd.apple.mpegurl".into(), playlist(5, "").into_bytes()));
        for i in 0..5 {
            r.insert(format!("/s{i}.ts"), ("video/mp2t".into(), seg_bytes(i)));
        }
        let site = serve_site(r).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("movie.ts");
        let seen = Arc::new(AtomicU64::new(0));
        let s2 = seen.clone();
        let out = download_hls(&Client::new(), &site.url("/master.m3u8"), &dest, &opts(), move |done, _| s2.store(done, Ordering::SeqCst)).await.unwrap();
        assert_eq!(out, dest);
        assert_eq!(std::fs::read(&dest).unwrap(), (0..5).flat_map(seg_bytes).collect::<Vec<u8>>());
        assert_eq!(seen.load(Ordering::SeqCst), 25_000);
        assert!(!dir.path().join("movie.ts.hls").exists(), "work folder is removed");
        assert!(!site.hits().iter().any(|p| p == "/low.m3u8"), "the lower variant is never fetched");
    }

    #[tokio::test]
    async fn decrypts_aes128_with_explicit_and_sequence_iv() {
        use cbc::cipher::BlockEncryptMut;
        let key = [7u8; 16];
        let iv_explicit = [9u8; 16];
        let enc = |data: &[u8], iv: &[u8; 16]| cbc::Encryptor::<aes::Aes128>::new((&key).into(), iv.into()).encrypt_padded_vec_mut::<Pkcs7>(data);
        let mut r = Routes::new();
        let pl = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:3\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n#EXTINF:4,\ns0.ts\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",IV=0x09090909090909090909090909090909\n#EXTINF:4,\ns1.ts\n#EXT-X-ENDLIST\n";
        r.insert("/p.m3u8".into(), ("application/x-mpegURL".into(), pl.as_bytes().to_vec()));
        r.insert("/key.bin".into(), ("application/octet-stream".into(), key.to_vec()));
        r.insert("/s0.ts".into(), ("video/mp2t".into(), enc(&seg_bytes(0), &(3u128).to_be_bytes())));
        r.insert("/s1.ts".into(), ("video/mp2t".into(), enc(&seg_bytes(1), &iv_explicit)));
        let site = serve_site(r).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("enc.ts");
        download_hls(&Client::new(), &site.url("/p.m3u8"), &dest, &opts(), |_, _| {}).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), [seg_bytes(0), seg_bytes(1)].concat());
        assert_eq!(site.hits().iter().filter(|p| *p == "/key.bin").count(), 1, "the key is fetched once");
    }

    #[tokio::test]
    async fn resumes_from_saved_segments_and_can_be_cancelled() {
        let mut r = Routes::new();
        r.insert("/p.m3u8".into(), ("application/x-mpegURL".into(), playlist(4, "").into_bytes()));
        for i in 0..4 {
            r.insert(format!("/s{i}.ts"), ("video/mp2t".into(), seg_bytes(i)));
        }
        let site = serve_site(r).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("r.ts");
        let work = dir.path().join("r.ts.hls");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(seg_path(&work, 0), seg_bytes(0)).unwrap();
        std::fs::write(seg_path(&work, 1), seg_bytes(1)).unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let o = HlsOptions { cancel, ..opts() };
        let err = download_hls(&Client::new(), &site.url("/p.m3u8"), &dest, &o, |_, _| {}).await.unwrap_err();
        assert!(err.downcast_ref::<Cancelled>().is_some(), "{err:#}");
        assert!(seg_path(&work, 0).exists(), "partial work is kept for the next try");

        download_hls(&Client::new(), &site.url("/p.m3u8"), &dest, &opts(), |_, _| {}).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), (0..4).flat_map(seg_bytes).collect::<Vec<u8>>());
        let hits = site.hits();
        assert!(!hits.iter().any(|p| p == "/s0.ts" || p == "/s1.ts"), "saved segments are not fetched again: {hits:?}");
    }

    #[tokio::test]
    async fn live_streams_and_missing_segments_fail_clearly() {
        let mut r = Routes::new();
        r.insert("/live.m3u8".into(), ("application/x-mpegURL".into(), b"#EXTM3U\n#EXTINF:4,\ns0.ts\n".to_vec()));
        r.insert("/gone.m3u8".into(), ("application/x-mpegURL".into(), playlist(2, "").into_bytes()));
        let site = serve_site(r).await;
        let dir = tempfile::tempdir().unwrap();
        let e = download_hls(&Client::new(), &site.url("/live.m3u8"), &dir.path().join("a.ts"), &opts(), |_, _| {}).await.unwrap_err();
        assert!(format!("{e:#}").contains("live stream"), "{e:#}");
        let e = download_hls(&Client::new(), &site.url("/gone.m3u8"), &dir.path().join("b.ts"), &opts(), |_, _| {}).await.unwrap_err();
        assert!(format!("{e:#}").contains("404"), "{e:#}");
    }

    /// End to end with a real stream made by ffmpeg: download, remux to MP4, and ask ffprobe about it.
    /// Skipped when ffmpeg/ffprobe are not installed.
    #[tokio::test]
    async fn real_ffmpeg_stream_becomes_a_playable_mp4() {
        let (Some(ff), true) = (ffmpeg_path(), std::process::Command::new("ffprobe").arg("-version").output().is_ok()) else {
            eprintln!("skipped: ffmpeg/ffprobe not installed");
            return;
        };
        let src = tempfile::tempdir().unwrap();
        let ok = std::process::Command::new(ff)
            .current_dir(src.path())
            .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i", "testsrc=duration=3:size=128x72:rate=10", "-f", "lavfi", "-i", "sine=duration=3", "-c:v", "libx264", "-c:a", "aac", "-f", "hls", "-hls_time", "1", "-hls_playlist_type", "vod", "-hls_segment_filename", "seg%d.ts", "index.m3u8"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            eprintln!("skipped: this ffmpeg cannot encode H.264/AAC");
            return;
        }
        let mut r = Routes::new();
        for e in std::fs::read_dir(src.path()).unwrap() {
            let e = e.unwrap();
            let name = e.file_name().to_string_lossy().into_owned();
            r.insert(format!("/{name}"), ("application/octet-stream".into(), std::fs::read(e.path()).unwrap()));
        }
        let site = serve_site(r).await;
        let out_dir = tempfile::tempdir().unwrap();
        let dest = out_dir.path().join("clip.mp4");
        let o = HlsOptions { remux: true, ..Default::default() };
        let got = download_hls(&Client::new(), &site.url("/index.m3u8"), &dest, &o, |_, _| {}).await.unwrap();
        assert_eq!(got, dest, "remux should succeed and keep the .mp4 name");
        let probe = std::process::Command::new("ffprobe").args(["-v", "error", "-show_entries", "format=format_name,duration", "-of", "default=nw=1"]).arg(&dest).output().unwrap();
        let text = String::from_utf8_lossy(&probe.stdout).to_string();
        assert!(text.contains("mp4"), "{text}");
        let dur: f64 = text.lines().find_map(|l| l.strip_prefix("duration=")?.parse().ok()).unwrap_or(0.0);
        assert!((2.5..3.8).contains(&dur), "duration {dur}: {text}");
    }
}
