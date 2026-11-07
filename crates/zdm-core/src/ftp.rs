//! FTP download: one stream, resumed with `REST` from the partial file when the server allows it.
//! Passive mode, plain FTP (no TLS). Anonymous login unless the link or the options carry a name.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use suppaftp::types::FileType;
use suppaftp::tokio::AsyncFtpStream;
use tokio::io::AsyncReadExt;
use url::Url;

use crate::engine::Ctx;
use crate::grab::percent_decode;
use crate::{Cancelled, Options};

pub fn is_ftp_url(url: &str) -> bool {
    url.len() > 6 && url[..6].eq_ignore_ascii_case("ftp://")
}

/// Decoded last path segment of an FTP link, for the file name.
pub fn file_name(url: &str) -> String {
    let n = Url::parse(url).ok().and_then(|u| u.path_segments().and_then(|mut s| s.next_back().map(percent_decode))).unwrap_or_default();
    if n.is_empty() { "download".into() } else { n }
}

fn part_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

/// Download `url` to `dest`. `on_progress(done, total)` (`total` 0 if the server does not say).
pub async fn download_ftp(url: &str, dest: &Path, opts: &Options, on_progress: impl Fn(u64, u64) + Send + Sync + 'static) -> Result<()> {
    let u = Url::parse(url).with_context(|| format!("bad address {url}"))?;
    if !is_ftp_url(url) {
        bail!("not an ftp:// address");
    }
    let host = u.host_str().ok_or_else(|| anyhow!("the address has no host"))?;
    let addr = format!("{host}:{}", u.port().unwrap_or(21));
    let path = percent_decode(u.path());
    let (user, pass) = match (u.username(), &opts.auth) {
        ("", Some((n, p))) => (n.clone(), p.clone()),
        ("", None) => ("anonymous".to_string(), "anonymous@".to_string()),
        (n, _) => (percent_decode(n), percent_decode(u.password().unwrap_or(""))),
    };
    let ctx = Ctx::from_opts(opts);
    let part = part_path(dest);

    let mut ftp = tokio::select! {
        _ = opts.cancel.cancelled() => return Err(Cancelled.into()),
        r = tokio::time::timeout(Duration::from_secs(20), AsyncFtpStream::connect(addr.clone())) => r.map_err(|_| anyhow!("timed out connecting to {addr}"))?.with_context(|| format!("cannot connect to {addr}"))?,
    };
    ftp.login(user.as_str(), pass.as_str()).await.map_err(|e| anyhow!("FTP login refused: {e}"))?;
    ftp.transfer_type(FileType::Binary).await.map_err(|e| anyhow!("{e}"))?;
    let total = ftp.size(&path).await.ok().map(|s| s as u64);

    let mut have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    if total.is_some_and(|t| have > t) {
        have = 0; // the file on the server is now smaller than our partial copy: start over
    }
    let mut attempt_from = have;
    let mut file;
    let mut stream = loop {
        if attempt_from > 0 {
            ftp.resume_transfer(attempt_from as usize).await.map_err(|e| anyhow!("{e}"))?;
        }
        match ftp.retr_as_stream(&path).await {
            Ok(s) => {
                file = std::fs::OpenOptions::new().create(true).write(true).truncate(attempt_from == 0).open(&part)?;
                if attempt_from > 0 {
                    use std::io::Seek;
                    file.seek(std::io::SeekFrom::Start(attempt_from))?;
                }
                break s;
            }
            // the server cannot restart: fetch everything again, once
            Err(_) if attempt_from > 0 => attempt_from = 0,
            Err(e) => bail!("FTP server refused the file: {e}"),
        }
    };

    let mut done = attempt_from;
    let mut buf = vec![0u8; 64 * 1024];
    let mut last = Instant::now();
    loop {
        let n = tokio::select! {
            _ = opts.cancel.cancelled() => { let _ = file.flush(); return Err(Cancelled.into()); }
            r = stream.read(&mut buf) => r?,
        };
        if n == 0 {
            break;
        }
        ctx.wait(n).await;
        file.write_all(&buf[..n])?;
        done += n as u64;
        if last.elapsed() >= Duration::from_millis(300) {
            on_progress(done, total.unwrap_or(0));
            last = Instant::now();
        }
    }
    drop(stream);
    file.flush()?;
    drop(file);
    if let Some(t) = total {
        if done != t {
            bail!("the connection ended after {done} of {t} bytes (the partial file is kept; resume to continue)");
        }
    }
    on_progress(done, total.unwrap_or(done));
    let _ = ftp.quit().await;
    std::fs::rename(&part, dest).with_context(|| format!("cannot move {} to {}", part.display(), dest.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testserver::{expected, start_ftp};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    fn files() -> HashMap<String, Vec<u8>> {
        HashMap::from([("/pub/data.bin".to_string(), expected(300_000)), ("/other.bin".to_string(), expected(1000))])
    }

    #[test]
    fn urls_and_names() {
        assert!(is_ftp_url("ftp://h/x") && is_ftp_url("FTP://h/x") && !is_ftp_url("http://h/x") && !is_ftp_url("ftp:/"));
        assert_eq!(file_name("ftp://h/pub/My%20File.iso"), "My File.iso");
        assert_eq!(file_name("ftp://h/"), "download");
    }

    #[tokio::test]
    async fn anonymous_download_reports_progress() {
        let srv = start_ftp(files(), None, true).await;
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("data.bin");
        let last = Arc::new(Mutex::new((0u64, 0u64)));
        let l2 = last.clone();
        download_ftp(&srv.url("/pub/data.bin"), &dest, &Options::default(), move |d, t| *l2.lock().unwrap() = (d, t)).await.unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), expected(300_000));
        assert_eq!(*last.lock().unwrap(), (300_000, 300_000));
        assert!(!part_path(&dest).exists());
        assert!(srv.commands().iter().any(|c| c.starts_with("USER anonymous")), "{:?}", srv.commands());
    }

    #[tokio::test]
    async fn login_from_link_or_options_and_wrong_password() {
        let srv = start_ftp(files(), Some(("carol".into(), "p@ss".into())), true).await;
        let dir = tempfile::tempdir().unwrap();
        // in the link, percent-encoded
        let a = dir.path().join("a.bin");
        let link = srv.url("/other.bin").replacen("ftp://", "ftp://carol:p%40ss@", 1);
        download_ftp(&link, &a, &Options::default(), |_, _| {}).await.unwrap();
        assert_eq!(std::fs::read(&a).unwrap(), expected(1000));
        // from the options
        let b = dir.path().join("b.bin");
        let o = Options { auth: Some(("carol".into(), "p@ss".into())), ..Options::default() };
        download_ftp(&srv.url("/other.bin"), &b, &o, |_, _| {}).await.unwrap();
        // wrong password
        let c = dir.path().join("c.bin");
        let o = Options { auth: Some(("carol".into(), "nope".into())), ..Options::default() };
        let e = download_ftp(&srv.url("/other.bin"), &c, &o, |_, _| {}).await.unwrap_err();
        assert!(format!("{e:#}").contains("login refused"), "{e:#}");
        assert!(!c.exists());
    }

    #[tokio::test]
    async fn resumes_with_rest_and_falls_back_when_restart_is_unsupported() {
        for allow_rest in [true, false] {
            let srv = start_ftp(files(), None, allow_rest).await;
            let dir = tempfile::tempdir().unwrap();
            let dest = dir.path().join("data.bin");
            std::fs::write(part_path(&dest), &expected(300_000)[..120_000]).unwrap(); // an earlier, interrupted try
            download_ftp(&srv.url("/pub/data.bin"), &dest, &Options::default(), |_, _| {}).await.unwrap();
            assert_eq!(std::fs::read(&dest).unwrap(), expected(300_000), "allow_rest={allow_rest}");
            // the client always tries to restart at the end of the partial file; a server that refuses
            // (502) just makes it fetch everything again, with a single RETR in the end
            let cmds = srv.commands();
            assert!(cmds.iter().any(|c| c == "REST 120000"), "{cmds:?}");
            assert_eq!(cmds.iter().filter(|c| c.starts_with("RETR")).count(), 1, "allow_rest={allow_rest}: {cmds:?}");
        }
    }

    #[tokio::test]
    async fn missing_file_and_cancel() {
        let srv = start_ftp(files(), None, true).await;
        let dir = tempfile::tempdir().unwrap();
        let e = download_ftp(&srv.url("/nope.bin"), &dir.path().join("n.bin"), &Options::default(), |_, _| {}).await.unwrap_err();
        assert!(format!("{e:#}").to_lowercase().contains("refused") || format!("{e:#}").contains("550"), "{e:#}");
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        let o = Options { cancel, ..Options::default() };
        let e = download_ftp(&srv.url("/other.bin"), &dir.path().join("c.bin"), &o, |_, _| {}).await.unwrap_err();
        assert!(e.downcast_ref::<Cancelled>().is_some());
    }
}
