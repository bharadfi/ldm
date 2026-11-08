//! Video pages through a fake yt-dlp (a shell script), so nothing here needs the internet.
//! Own test binary: it sets `ZDM_YTDLP` for the whole process.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use zdm_proto::{AddRequest, State, TaskInfo};
use zdmd::config::Config;
use zdmd::manager::Manager;
use zdmd::store::Store;

const FAKE: &str = r#"#!/bin/sh
# stands in for yt-dlp: `-J` prints a saved format list, anything else "downloads"
echo "$*" >> "${ZDM_ARGLOG:-/dev/null}"
for a in "$@"; do url="$a"; done
key=$(printf %s "$url" | tr -c 'A-Za-z0-9' '_')
prev=""
for a in "$@"; do
  [ "$prev" = "--cookies" ] && cp "$a" "$ZDM_LOGDIR/cookies-$key"
  prev="$a"
  case "$a" in
    *BOTCHECK*) echo "ERROR: [youtube] x: Sign in to confirm you’re not a bot. Use --cookies-from-browser or --cookies for the authentication." >&2; exit 1;;
    *FAILURL*) echo "ERROR: [fake] Unsupported URL: $a" >&2; exit 1;;
  esac
done
for a in "$@"; do
  if [ "$a" = "-J" ]; then echo x >> "$ZDM_LOGDIR/count-$key"; cat "$ZDM_FIXTURE"; exit 0; fi
done
out=""; sel=""; prev=""; allargs="$*"
for a in "$@"; do
  [ "$prev" = "-o" ] && out="$a"
  [ "$prev" = "-f" ] && sel="$a"
  prev="$a"
done
echo "$allargs" > "$(dirname "$out")/args.txt"
case "$sel" in
  *UNAVAILABLE*) echo "ERROR: [fake] Video unavailable" >&2; exit 1;;
  *SLOW*) sleep 30;;
esac
# two streams, one after the other, as for video + audio
echo "ZDMP 1000 4000 NA"; echo "ZDMP 4000 4000 NA"; echo "ZDMP 500 1000 NA"; echo "ZDMP 1000 1000 NA"
case "$allargs" in
  *--write-subs*|*--write-auto-subs*) printf '1\n00:00:00,000 --> 00:00:01,000\nhello\n' > "$(dirname "$out")/out.en.srt";;
  *-x*) printf 'FAKEAUDIO' > "$(dirname "$out")/out.mp3";;
  *) printf 'FAKEVIDEO-%s' "$sel" > "$(dirname "$out")/out.mp4";;
esac
"#;

static SETUP: Once = Once::new();
static mut DIR: Option<PathBuf> = None;

fn setup() {
    SETUP.call_once(|| {
        let dir = std::env::temp_dir().join(format!("zdm-fake-ytdlp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("yt-dlp");
        std::fs::write(&script, FAKE).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::env::set_var("ZDM_YTDLP", &script);
        std::env::set_var("ZDM_ARGLOG", dir.join("args.log"));
        std::env::set_var("ZDM_LOGDIR", &dir);
        std::env::set_var("ZDM_FIXTURE", concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/ytdlp-formats.json"));
        #[allow(static_mut_refs)]
        unsafe {
            DIR = Some(dir);
        }
    });
}

struct Env {
    m: Arc<Manager>,
    root: tempfile::TempDir,
    stop: tokio_util::sync::CancellationToken,
    run: tokio::task::JoinHandle<()>,
}

fn env() -> Env {
    setup();
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
    Env { m, root, stop, run }
}

impl Env {
    fn add(&self, url: &str, sel: &str, name: &str) -> TaskInfo {
        self.m.add(AddRequest { url: url.into(), ytdlp: Some(sel.into()), filename: Some(name.into()), start: true, ..Default::default() }).unwrap()
    }
    async fn finished(&self, id: i64) -> TaskInfo {
        let t0 = Instant::now();
        loop {
            let t = self.m.get(id).unwrap();
            if matches!(t.state, State::Completed | State::Failed) || t0.elapsed() > Duration::from_secs(15) {
                return t;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }
    async fn stop(self) {
        self.stop.cancel();
        let _ = self.run.await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn page_lookup_returns_the_curated_menu_and_reports_failures() {
    let e = env();
    let l = e.m.formats("https://video.test/watch?v=abc", None).await.unwrap();
    assert!(l.options.iter().any(|o| o.id == "299+140" && o.label.contains("1080p")), "{:?}", l.options.iter().map(|o| &o.label).collect::<Vec<_>>());
    let err = e.m.formats("https://video.test/FAILURL", None).await.unwrap_err();
    assert!(format!("{err:#}").contains("Unsupported URL"), "{err:#}");
    assert!(e.m.formats("ftp://x/y", None).await.is_err());
    e.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn chosen_format_downloads_into_the_video_folder_with_summed_progress() {
    let e = env();
    let t = e.add("https://video.test/watch?v=abc", "299+140", "Clip [1080p].mp4");
    let t = e.finished(t.id).await;
    assert_eq!(t.state, State::Completed, "{:?}", t.error);
    let dest = t.dest.clone().unwrap();
    assert_eq!(Path::new(&dest), e.root.path().join("dl/Video/Clip [1080p].mp4"));
    assert_eq!(std::fs::read(&dest).unwrap(), b"FAKEVIDEO-299+140");
    assert_eq!(t.size, Some(17));
    assert!(!Path::new(&format!("{dest}.ytdlp")).exists(), "work folder removed");
    assert!(!serde_json::to_string(&t).unwrap().contains("x-zdm"), "internal headers are not shown");
    e.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn passes_the_right_options_to_yt_dlp() {
    let e = env();
    e.m.set_settings(zdm_proto::Settings { speed_limit_kib: 500, global_limit_kib: 300, proxy: Some("socks5://127.0.0.1:9".into()), ..e.m.settings() }).unwrap();
    let t = e.m.add(AddRequest {
        url: "https://video.test/watch?v=args".into(),
        ytdlp: Some("SLOWARGS".into()),
        filename: Some("Args.mp4".into()),
        referer: Some("https://video.test/".into()),
        headers: vec![("Cookie".into(), "a=b".into())],
        start: true,
        ..Default::default()
    }).unwrap();
    // wait until the fake has written its arguments (it then sleeps), then pause to stop it
    let args = {
        let t0 = Instant::now();
        loop {
            let work = e.m.get(t.id).unwrap().dest.map(|d| format!("{d}.ytdlp/args.txt"));
            if let Some(Ok(a)) = work.map(std::fs::read_to_string) {
                break a;
            }
            assert!(t0.elapsed() < Duration::from_secs(10), "yt-dlp never started");
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    };
    for needle in ["-f SLOWARGS", "--merge-output-format mp4", "--limit-rate 300K", "--proxy socks5://127.0.0.1:9", "--referer https://video.test/", "--cookies /", "--no-playlist", "-- https://video.test/watch?v=args"] {
        assert!(args.contains(needle), "missing {needle:?} in: {args}");
    }
    assert!(!args.contains("x-zdm") && !args.contains("--add-headers Cookie"), "the Cookie header goes in a cookie file, not a header: {args}");
    e.m.pause(t.id).unwrap();
    let t0 = Instant::now();
    while e.m.get(t.id).unwrap().state != State::Paused || t0.elapsed() < Duration::from_millis(300) {
        assert!(t0.elapsed() < Duration::from_secs(8), "pause did not stop yt-dlp");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    e.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn audio_conversion_and_subtitles_produce_the_right_files() {
    let e = env();
    let a = e.finished(e.add("https://video.test/watch?v=a", "x:mp3", "Song [audio].mp3").id).await;
    assert_eq!(a.state, State::Completed, "{:?}", a.error);
    assert_eq!(std::fs::read(a.dest.unwrap()).unwrap(), b"FAKEAUDIO");
    let s = e.finished(e.add("https://video.test/watch?v=a", "sub:en", "Song [en].srt").id).await;
    assert_eq!(s.state, State::Completed, "{:?}", s.error);
    assert!(std::fs::read_to_string(s.dest.clone().unwrap()).unwrap().contains("hello"));
    assert_eq!(s.category.as_deref(), Some("Other"));
    e.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn formats_of_one_page_are_separate_downloads_and_final_errors_are_not_retried() {
    let e = env();
    let url = "https://video.test/watch?v=same";
    let a = e.add(url, "UNAVAILABLE1", "A.mp4");
    let b = e.add(url, "UNAVAILABLE2", "B.mp4");
    assert_ne!(a.id, b.id, "another format of the same page is another download");
    assert_eq!(e.add(url, "UNAVAILABLE1", "A.mp4").id, a.id, "the same format is not queued twice");
    let a = e.finished(a.id).await;
    assert_eq!(a.state, State::Failed);
    assert!(a.error.as_deref().unwrap().contains("Video unavailable"), "{:?}", a.error);
    assert_eq!(e.m.retry_count(), 0, "an unavailable video is not retried");
    assert!(e.m.add(AddRequest { url: url.into(), ytdlp: Some("--exec rm".into()), start: true, ..Default::default() }).is_err(), "a selector must not look like an option");
    e.stop().await;
}

fn log_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("ZDM_LOGDIR").unwrap())
}

/// Logs of the fake are kept per page address (tests run in parallel).
fn key(url: &str) -> String {
    url.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn browser_cookies_reach_yt_dlp_in_a_private_file_that_is_removed_afterwards() {
    let e = env();
    let url = "https://cookies.test/watch?v=one";
    e.m.formats(url, Some("VISITOR=abc; CONSENT=yes")).await.unwrap();
    let args = std::fs::read_to_string(log_dir().join("args.log")).unwrap();
    let line = args.lines().filter(|l| l.contains("--cookies ") && l.contains("cookies.test/watch?v=one")).next_back().expect("yt-dlp was given a cookie file");
    let path = line.split("--cookies ").nth(1).unwrap().split_whitespace().next().unwrap();
    let copy = std::fs::read_to_string(log_dir().join(format!("cookies-{}", key(url)))).unwrap();
    assert!(copy.contains(".cookies.test\tTRUE\t/\tTRUE") && copy.contains("\tVISITOR\tabc") && copy.contains("\tCONSENT\tyes"), "{copy}");
    assert!(!std::path::Path::new(path).exists(), "the cookie file must be deleted once yt-dlp is done: {path}");
    e.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn chosen_browser_cookies_are_used_only_when_the_page_sent_none() {
    let e = env();
    let mut s = e.m.settings();
    s.ytdlp_cookies_browser = Some("firefox".into());
    e.m.set_settings(s).unwrap();
    e.m.formats("https://browser-cookies.test/watch?v=a", None).await.unwrap();
    e.m.formats("https://browser-cookies.test/watch?v=b", Some("x=y")).await.unwrap();
    let args = std::fs::read_to_string(log_dir().join("args.log")).unwrap();
    let a = args.lines().find(|l| l.contains("browser-cookies.test/watch?v=a")).unwrap();
    let b = args.lines().find(|l| l.contains("browser-cookies.test/watch?v=b")).unwrap();
    assert!(a.contains("--cookies-from-browser firefox") && !a.contains("--cookies /"), "{a}");
    assert!(b.contains("--cookies /") && !b.contains("--cookies-from-browser"), "{b}");
    assert!(e.m.set_settings(zdm_proto::Settings { ytdlp_cookies_browser: Some("netscape".into()), ..e.m.settings() }).is_err());
    e.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_same_page_is_looked_up_once_and_a_bot_check_is_explained_and_not_retried() {
    let e = env();
    let url = "https://cache.test/watch?v=twice";
    let counter = log_dir().join(format!("count-{}", key(url)));
    let a = e.m.formats(url, None).await.unwrap();
    let b = e.m.formats(url, None).await.unwrap();
    assert_eq!(a, b);
    assert_eq!(std::fs::read_to_string(counter).unwrap().lines().count(), 1, "the second lookup comes from the cache");

    let err = e.m.formats("https://video.test/BOTCHECK", None).await.unwrap_err();
    assert!(format!("{err:#}").contains("not a robot") && format!("{err:#}").contains("Options"), "{err:#}");
    // as a download it fails once, in plain words, and is not retried (retrying would make it worse)
    let t = e.m.add(AddRequest { url: "https://video.test/BOTCHECK".into(), ytdlp: Some("299+140".into()), filename: Some("x.mp4".into()), start: true, ..Default::default() }).unwrap();
    let t = e.finished(t.id).await;
    assert_eq!(t.state, State::Failed);
    assert!(t.error.as_deref().unwrap().contains("not a robot"), "{:?}", t.error);
    assert_eq!(e.m.retry_count(), 0);
    e.stop().await;
}
