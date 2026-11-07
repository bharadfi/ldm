//! DASH (`.mpd`) download through ffmpeg, which understands the format: it fetches the segments, picks the best
//! video and audio, and writes one MP4. We add what the rest of the app expects: progress, cancel, the request
//! headers/cookies/referer of the browser, a proxy, and clear errors.
//!
//! Unlike HLS there is no partial resume: a paused or interrupted DASH download starts again from the beginning.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use reqwest::Client;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use url::Url;

use crate::hls::ffmpeg_path;
use crate::{Cancelled, Options};

pub fn is_dash_url(url: &str) -> bool {
    Url::parse(url).map(|u| u.path().to_ascii_lowercase().ends_with(".mpd")).unwrap_or(false)
}

/// ISO 8601 duration as used by DASH (`PT1H2M3.5S`, `P1DT2H`) in seconds.
pub fn parse_iso_duration(s: &str) -> Option<f64> {
    let s = s.trim().strip_prefix('P')?;
    let (date, time) = s.split_once('T').unwrap_or((s, ""));
    let mut total = 0.0;
    for (part, units) in [(date, [('D', 86400.0)].as_slice()), (time, [('H', 3600.0), ('M', 60.0), ('S', 1.0)].as_slice())] {
        let mut num = String::new();
        for c in part.chars() {
            if c.is_ascii_digit() || c == '.' {
                num.push(c);
            } else {
                let (_, mult) = units.iter().find(|(u, _)| *u == c)?;
                total += num.parse::<f64>().ok()? * mult;
                num.clear();
            }
        }
        if !num.is_empty() {
            return None;
        }
    }
    Some(total)
}

/// (is a live stream, total duration in seconds) from the manifest text.
pub fn manifest_info(xml: &str) -> (bool, Option<f64>) {
    let attr = |name: &str| {
        let i = xml.find(&format!("{name}=\""))?;
        let rest = &xml[i + name.len() + 2..];
        Some(rest[..rest.find('"')?].to_string())
    };
    (attr("type").is_some_and(|t| t == "dynamic"), attr("mediaPresentationDuration").and_then(|d| parse_iso_duration(&d)))
}

/// `Name: value\r\n` lines for ffmpeg's `-headers`: the browser's headers, referer and Basic sign-in.
fn header_block(opts: &Options) -> String {
    use base64::Engine;
    let mut h = String::new();
    for (k, v) in &opts.headers {
        h += &format!("{k}: {v}\r\n");
    }
    if let Some(r) = &opts.referer {
        h += &format!("Referer: {r}\r\n");
    }
    if let Some((u, p)) = &opts.auth {
        h += &format!("Authorization: Basic {}\r\n", base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}")));
    }
    h
}

/// Download the DASH stream at `url` to `dest` (MP4). `on_progress(bytes_written, estimated_total)`.
pub async fn download_dash(client: &Client, url: &str, dest: &Path, opts: &Options, on_progress: impl Fn(u64, u64) + Send + Sync + 'static) -> Result<()> {
    let ffmpeg = ffmpeg_path().ok_or_else(|| anyhow!("DASH video needs ffmpeg installed (sudo apt install ffmpeg)"))?;
    let text = tokio::select! {
        _ = opts.cancel.cancelled() => return Err(Cancelled.into()),
        r = async { client.get(url).send().await?.error_for_status()?.text().await } => r.with_context(|| format!("cannot read {url}"))?,
    };
    let (live, duration) = manifest_info(&text);
    if live {
        bail!("this is a live stream (type=dynamic); live streams are not supported");
    }
    let tmp: PathBuf = {
        let mut s = dest.as_os_str().to_owned();
        s.push(".part");
        s.into()
    };
    let mut cmd = tokio::process::Command::new(ffmpeg);
    cmd.args(["-nostdin", "-y", "-loglevel", "error", "-nostats", "-progress", "pipe:1", "-user_agent", &opts.user_agent]);
    let headers = header_block(opts);
    if !headers.is_empty() {
        cmd.args(["-headers", &headers]);
    }
    if let Some(p) = &opts.proxy {
        cmd.args(["-http_proxy", p]);
    }
    cmd.args(["-i", url, "-c", "copy", "-f", "mp4"]).arg(&tmp).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let mut child = cmd.spawn().context("could not start ffmpeg")?;
    let mut lines = BufReader::new(child.stdout.take().expect("piped")).lines();
    let mut stderr = child.stderr.take().expect("piped");
    let err_text = tokio::spawn(async move {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s).await;
        s
    });

    let status = loop {
        tokio::select! {
            _ = opts.cancel.cancelled() => {
                let _ = child.kill().await;
                let _ = std::fs::remove_file(&tmp);
                return Err(Cancelled.into());
            }
            line = lines.next_line() => match line {
                Ok(Some(l)) => {
                    if let Some(us) = l.strip_prefix("out_time_us=").and_then(|v| v.trim().parse::<f64>().ok()) {
                        let done = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
                        let frac = duration.map_or(0.0, |d| (us / 1e6 / d).clamp(0.0, 1.0));
                        on_progress(done, if frac > 0.02 { (done as f64 / frac) as u64 } else { 0 });
                    }
                }
                _ => break child.wait().await?,
            },
            _ = tokio::time::sleep(Duration::from_secs(3600)) => {}
        }
    };
    if !status.success() {
        let _ = std::fs::remove_file(&tmp);
        let e = err_text.await.unwrap_or_default();
        let tail: String = e.lines().rev().take(3).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join(" | ");
        bail!("ffmpeg could not download the stream: {}", if tail.is_empty() { status.to_string() } else { tail });
    }
    let len = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
    if len == 0 {
        let _ = std::fs::remove_file(&tmp);
        bail!("ffmpeg produced an empty file");
    }
    on_progress(len, len);
    std::fs::rename(&tmp, dest).with_context(|| format!("cannot move {} to {}", tmp.display(), dest.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testserver::{serve_site, Routes};

    #[test]
    fn durations_and_manifest_facts() {
        assert_eq!(parse_iso_duration("PT3S"), Some(3.0));
        assert_eq!(parse_iso_duration("PT1H2M3.5S"), Some(3723.5));
        assert_eq!(parse_iso_duration("P1DT1H"), Some(90000.0));
        assert_eq!(parse_iso_duration("PT0H0M6.000S"), Some(6.0));
        assert_eq!(parse_iso_duration("3S"), None);
        assert_eq!(parse_iso_duration("PT5"), None);
        let mpd = r#"<MPD type="static" mediaPresentationDuration="PT0H0M12.5S" minBufferTime="PT1.5S">"#;
        assert_eq!(manifest_info(mpd), (false, Some(12.5)));
        assert_eq!(manifest_info(r#"<MPD type="dynamic" minimumUpdatePeriod="PT2S">"#), (true, None));
        assert!(is_dash_url("https://x.test/v/stream.MPD?token=1") && !is_dash_url("https://x.test/a.m3u8"));
    }

    #[tokio::test]
    async fn live_manifests_and_dead_links_fail_clearly() {
        if ffmpeg_path().is_none() {
            eprintln!("skipped: no ffmpeg");
            return;
        }
        let mut r = Routes::new();
        r.insert("/live.mpd".into(), ("application/dash+xml".into(), br#"<MPD type="dynamic"></MPD>"#.to_vec()));
        let site = serve_site(r).await;
        let dir = tempfile::tempdir().unwrap();
        let e = download_dash(&Client::new(), &site.url("/live.mpd"), &dir.path().join("a.mp4"), &Options::default(), |_, _| {}).await.unwrap_err();
        assert!(format!("{e:#}").contains("live stream"), "{e:#}");
        let e = download_dash(&Client::new(), &site.url("/none.mpd"), &dir.path().join("b.mp4"), &Options::default(), |_, _| {}).await.unwrap_err();
        assert!(format!("{e:#}").contains("404"), "{e:#}");
    }

    /// A real DASH stream made by ffmpeg, downloaded with progress, checked with ffprobe.
    #[tokio::test]
    async fn real_dash_stream_becomes_a_playable_mp4() {
        let (Some(ff), true) = (ffmpeg_path(), std::process::Command::new("ffprobe").arg("-version").output().is_ok()) else {
            eprintln!("skipped: ffmpeg/ffprobe not installed");
            return;
        };
        let src = tempfile::tempdir().unwrap();
        let ok = std::process::Command::new(ff)
            .current_dir(src.path())
            .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i", "testsrc=duration=4:size=160x90:rate=10", "-f", "lavfi", "-i", "sine=duration=4", "-c:v", "libx264", "-c:a", "aac", "-f", "dash", "-seg_duration", "1", "manifest.mpd"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            eprintln!("skipped: this ffmpeg cannot make a DASH test stream");
            return;
        }
        let mut r = Routes::new();
        for e in std::fs::read_dir(src.path()).unwrap() {
            let e = e.unwrap();
            let name = e.file_name().to_string_lossy().into_owned();
            let ct = if name.ends_with(".mpd") { "application/dash+xml" } else { "video/iso.segment" };
            r.insert(format!("/{name}"), (ct.into(), std::fs::read(e.path()).unwrap()));
        }
        let site = serve_site(r).await;
        let out = tempfile::tempdir().unwrap();
        let dest = out.path().join("clip.mp4");
        let last = std::sync::Arc::new(std::sync::Mutex::new((0u64, 0u64)));
        let l2 = last.clone();
        download_dash(&Client::new(), &site.url("/manifest.mpd"), &dest, &Options::default(), move |d, t| *l2.lock().unwrap() = (d, t)).await.unwrap();
        let probe = std::process::Command::new("ffprobe").args(["-v", "error", "-show_entries", "format=format_name,duration:stream=codec_type", "-of", "default=nw=1"]).arg(&dest).output().unwrap();
        let text = String::from_utf8_lossy(&probe.stdout).to_string();
        assert!(text.contains("mp4") && text.contains("codec_type=video") && text.contains("codec_type=audio"), "{text}");
        let dur: f64 = text.lines().find_map(|l| l.strip_prefix("duration=")?.parse().ok()).unwrap_or(0.0);
        assert!((3.3..4.8).contains(&dur), "duration {dur}: {text}");
        let (d, t) = *last.lock().unwrap();
        assert!(d > 0 && d == t, "the last report is the final size: {d} of {t}");
        assert!(!dest.with_extension("mp4.part").exists());

        // cancelling stops ffmpeg and leaves nothing behind
        let o = Options { cancel: crate::CancellationToken::new(), ..Options::default() };
        o.cancel.cancel();
        let e = download_dash(&Client::new(), &site.url("/manifest.mpd"), &out.path().join("x.mp4"), &o, |_, _| {}).await.unwrap_err();
        assert!(e.downcast_ref::<Cancelled>().is_some());
    }
}
