use std::time::Instant;

use super::*;
use crate::testserver::{expected, start, Config};

const MIB: u64 = 1 << 20;

fn cfg(size: u64) -> Config {
    Config { size, ranges: true, ..Config::default() }
}

fn opts() -> Options {
    Options { retries: 4, ..Options::default() }
}

async fn run(url: &str, dest: &Path, o: &Options) -> Result<()> {
    download(url, dest, o, |_, _| {}).await
}

fn assert_file(dest: &Path, size: u64) {
    let got = std::fs::read(dest).unwrap();
    assert_eq!(got.len() as u64, size);
    assert!(got == expected(size), "content differs");
    assert!(!sibling(dest, ".part").exists(), "partial file left behind");
    assert!(!sibling(dest, ".part.zdm").exists(), "sidecar left behind");
}

#[tokio::test(flavor = "multi_thread")]
async fn parallel_download_is_correct_and_uses_many_connections() {
    let size = 12 * MIB + 12345;
    let srv = start(cfg(size)).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("f.bin");
    run(&srv.url("/f.bin"), &dest, &opts()).await.unwrap();
    assert_file(&dest, size);
    assert!(srv.range_starts().len() >= 4, "expected several ranged requests, got {:?}", srv.range_starts());
}

#[tokio::test(flavor = "multi_thread")]
async fn server_without_ranges_falls_back_to_single_stream() {
    let size = 3 * MIB;
    let srv = start(Config { ranges: false, ..cfg(size) }).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("f.bin");
    run(&srv.url("/f.bin"), &dest, &opts()).await.unwrap();
    assert_file(&dest, size);
    assert!(srv.range_starts().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn transient_503_is_retried() {
    let size = 6 * MIB;
    let srv = start(Config { fail_first_ranged: 3, ..cfg(size) }).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("f.bin");
    run(&srv.url("/f.bin"), &dest, &opts()).await.unwrap();
    assert_file(&dest, size);
    assert!(srv.requests().iter().filter(|r| r.status == 503).count() == 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn persistent_503_fails_and_keeps_resume_state() {
    let size = 6 * MIB;
    let srv = start(Config { fail_first_ranged: usize::MAX, ..cfg(size) }).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("f.bin");
    let o = Options { retries: 1, ..opts() };
    assert!(run(&srv.url("/f.bin"), &dest, &o).await.is_err());
    assert!(!dest.exists());
    assert!(sibling(&dest, ".part.zdm").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn basic_auth_is_sent_and_required() {
    let size = 2 * MIB;
    let expected_header = {
        use base64::Engine;
        format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("alice:s3cret"))
    };
    let srv = start(Config { auth: Some(expected_header), ..cfg(size) }).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("f.bin");
    assert!(run(&srv.url("/f.bin"), &dest, &opts()).await.is_err(), "must fail without credentials");
    let o = Options { auth: Some(("alice".into(), "s3cret".into())), ..opts() };
    run(&srv.url("/f.bin"), &dest, &o).await.unwrap();
    assert_file(&dest, size);
}

#[tokio::test(flavor = "multi_thread")]
async fn resume_requests_only_missing_bytes() {
    let size = 8 * MIB;
    let srv = start(cfg(size)).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("f.bin");
    let half = 4 * MIB;

    // Simulate an interrupted run: first half is on disk and recorded.
    let mut part = expected(size);
    part[half as usize..].fill(0);
    std::fs::write(sibling(&dest, ".part"), &part).unwrap();
    let sc = Sidecar {
        url: srv.url("/f.bin"),
        size,
        etag: Some("\"v1\"".into()),
        last_modified: None,
        segs: vec![Seg { start: 0, end: size, pos: half }],
    };
    std::fs::write(sibling(&dest, ".part.zdm"), serde_json::to_vec(&sc).unwrap()).unwrap();

    run(&srv.url("/f.bin"), &dest, &opts()).await.unwrap();
    assert_file(&dest, size);
    let starts = srv.range_starts();
    assert!(!starts.is_empty() && starts.iter().all(|&s| s >= half), "re-downloaded saved bytes: {starts:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_sidecar_with_different_etag_is_ignored() {
    let size = 4 * MIB;
    let srv = start(cfg(size)).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("f.bin");
    // A sidecar for another version claims everything is done; garbage bytes on disk must not be trusted.
    std::fs::write(sibling(&dest, ".part"), vec![7u8; size as usize]).unwrap();
    let sc = Sidecar {
        url: srv.url("/f.bin"),
        size,
        etag: Some("\"old\"".into()),
        last_modified: None,
        segs: vec![Seg { start: 0, end: size, pos: size }],
    };
    std::fs::write(sibling(&dest, ".part.zdm"), serde_json::to_vec(&sc).unwrap()).unwrap();
    run(&srv.url("/f.bin"), &dest, &opts()).await.unwrap();
    assert_file(&dest, size);
}

#[tokio::test(flavor = "multi_thread")]
async fn file_replaced_mid_download_restarts_once() {
    let size = 5 * MIB;
    // First request (probe) sees v1, everything after sees v2, so If-Range: v1 fails with a full 200.
    let srv = start(Config { flip_etag: true, ..cfg(size) }).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("f.bin");
    run(&srv.url("/f.bin"), &dest, &opts()).await.unwrap();
    assert_file(&dest, size);
    let probes = srv.requests().iter().filter(|r| r.range.as_deref() == Some("bytes=0-0")).count();
    assert_eq!(probes, 2, "engine must re-probe after detecting the change");
}

#[tokio::test(flavor = "multi_thread")]
async fn rate_limit_slows_download() {
    let size = 3 * MIB;
    let srv = start(cfg(size)).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("f.bin");
    let o = Options { limit_bps: Some(MIB), ..opts() };
    let t = Instant::now();
    run(&srv.url("/f.bin"), &dest, &o).await.unwrap();
    assert!(t.elapsed().as_secs_f64() >= 1.5, "finished in {:?}, limit not applied", t.elapsed());
    assert_file(&dest, size);
}

#[tokio::test(flavor = "multi_thread")]
async fn filename_comes_from_content_disposition() {
    let srv = start(Config { disposition: Some("attachment; filename=\"a b.bin\"".into()), ..cfg(1000) }).await;
    let client = build_client(&opts()).unwrap();
    let p = probe(&client, &srv.url("/x")).await.unwrap();
    assert_eq!((p.filename.as_str(), p.size, p.ranges), ("a b.bin", Some(1000), true));
}
