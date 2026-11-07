//! DASH through the daemon. Needs ffmpeg + ffprobe (skipped without them). Own test binary: it must not share
//! process-wide environment variables with the HLS tests in manager.rs.

use std::sync::Arc;
use std::time::{Duration, Instant};

use zdm_core::testserver::{serve_site, Routes};
use zdm_proto::{AddRequest, State};
use zdmd::config::Config;
use zdmd::manager::Manager;
use zdmd::store::Store;

fn tool(name: &str) -> bool {
    std::process::Command::new(name).arg("-version").output().is_ok()
}

#[tokio::test(flavor = "multi_thread")]
async fn dash_link_becomes_an_mp4_in_the_video_folder() {
    if !tool("ffmpeg") || !tool("ffprobe") {
        eprintln!("skipped: ffmpeg/ffprobe not installed");
        return;
    }
    let src = tempfile::tempdir().unwrap();
    let made = std::process::Command::new("ffmpeg")
        .current_dir(src.path())
        .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i", "testsrc=duration=3:size=160x90:rate=10", "-f", "lavfi", "-i", "sine=duration=3", "-c:v", "libx264", "-c:a", "aac", "-f", "dash", "-seg_duration", "1", "stream.mpd"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !made {
        eprintln!("skipped: this ffmpeg cannot make a DASH test stream");
        return;
    }
    let mut routes = Routes::new();
    for e in std::fs::read_dir(src.path()).unwrap() {
        let e = e.unwrap();
        let name = e.file_name().to_string_lossy().into_owned();
        routes.insert(format!("/shows/pilot/{name}"), ("application/octet-stream".into(), std::fs::read(e.path()).unwrap()));
    }
    let site = serve_site(routes).await;

    let root = tempfile::tempdir().unwrap();
    let cfg = Config {
        db_path: root.path().join("t.db"),
        download_root: root.path().join("dl"),
        connections: 4,
        limit_bps: None,
        tick: Duration::from_millis(50),
        retry_delay: Duration::from_millis(50),
        after_grace: Duration::from_millis(100),
    };
    let m = Manager::with_store(cfg, Store::open_in_memory().unwrap(), Arc::new(|| (0, 0)));
    let stop = tokio_util::sync::CancellationToken::new();
    let run = tokio::spawn(m.clone().run(stop.clone()));
    m.add(AddRequest { url: site.url("/shows/pilot/stream.mpd"), start: true, ..Default::default() }).unwrap();
    let t0 = Instant::now();
    let t = loop {
        let t = m.list().unwrap().remove(0);
        if matches!(t.state, State::Completed | State::Failed) || t0.elapsed() > Duration::from_secs(30) {
            break t;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(t.state, State::Completed, "{:?}", t.error);
    let dest = t.dest.clone().unwrap();
    assert!(dest.ends_with("Video/pilot.mp4"), "{dest}");
    assert_eq!(t.size, Some(std::fs::metadata(&dest).unwrap().len()));
    let probe = std::process::Command::new("ffprobe").args(["-v", "error", "-show_entries", "format=duration", "-of", "default=nw=1:nk=1"]).arg(&dest).output().unwrap();
    let dur: f64 = String::from_utf8_lossy(&probe.stdout).trim().parse().unwrap_or(0.0);
    assert!((2.4..4.2).contains(&dur), "duration {dur}");
    stop.cancel();
    let _ = run.await;
}
