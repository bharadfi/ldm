use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::BufReader;
use tokio_util::sync::CancellationToken;
use zdm_core::testserver::{self, expected, Config as ServerCfg};
use zdm_proto::{roundtrip, AddRequest, QueueInfo, Request, Response, State, TaskInfo, Window};
use zdmd::config::Config;
use zdmd::manager::Manager;
use zdmd::server::serve_conn;
use zdmd::store::{NewTask, Store};

const MIB: u64 = 1 << 20;

struct Env {
    m: Arc<Manager>,
    root: tempfile::TempDir,
    clock: Arc<Mutex<(u32, u32)>>,
    shutdown: CancellationToken,
    run: tokio::task::JoinHandle<()>,
}

impl Env {
    fn new(limit_bps: Option<u64>) -> Env {
        let root = tempfile::tempdir().unwrap();
        let cfg = Config {
            db_path: root.path().join("unused.db"),
            download_root: root.path().join("dl"),
            connections: 4,
            limit_bps,
            tick: Duration::from_millis(50),
            retry_delay: Duration::from_millis(50),
            after_grace: Duration::from_millis(150),
        };
        let clock = Arc::new(Mutex::new((12 * 60, 2))); // Wednesday noon
        let c2 = clock.clone();
        let m = Manager::with_store(cfg, Store::open_in_memory().unwrap(), Arc::new(move || *c2.lock().unwrap()));
        let shutdown = CancellationToken::new();
        let run = tokio::spawn(m.clone().run(shutdown.clone()));
        Env { m, root, clock, shutdown, run }
    }

    fn dl(&self) -> std::path::PathBuf {
        self.root.path().join("dl")
    }

    fn add(&self, url: String) -> TaskInfo {
        self.m.add(AddRequest { url, start: true, ..Default::default() }).unwrap()
    }

    async fn until(&self, what: &str, f: impl Fn(&[TaskInfo]) -> bool) -> Vec<TaskInfo> {
        let t = Instant::now();
        loop {
            let tasks = self.m.list().unwrap();
            if f(&tasks) {
                return tasks;
            }
            assert!(t.elapsed() < Duration::from_secs(30), "timed out waiting for {what}: {tasks:#?}");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn stop(self) {
        self.shutdown.cancel();
        let _ = self.run.await;
    }
}

fn files_under(dir: &Path) -> Vec<String> {
    let mut v = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                v.extend(files_under(&p));
            } else {
                v.push(p.file_name().unwrap().to_string_lossy().into_owned());
            }
        }
    }
    v.sort();
    v
}

fn server_cfg(size: u64, name: &str) -> ServerCfg {
    ServerCfg { size, ranges: true, disposition: Some(format!("attachment; filename=\"{name}\"")), ..Default::default() }
}

#[tokio::test(flavor = "multi_thread")]
async fn completes_into_category_folder() {
    let srv = testserver::start(server_cfg(3 * MIB, "movie.mp4")).await;
    let env = Env::new(None);
    let t = env.add(srv.url("/a"));
    let tasks = env.until("completion", |ts| ts.iter().all(|t| t.state == State::Completed)).await;
    let dest = tasks[0].dest.clone().unwrap();
    assert_eq!(Path::new(&dest), env.dl().join("Video").join("movie.mp4"));
    assert_eq!(tasks[0].category.as_deref(), Some("Video"));
    assert_eq!(tasks[0].downloaded, 3 * MIB);
    assert_eq!(std::fs::read(dest).unwrap(), expected(3 * MIB));
    // the scheduler may already have started it by the time `add` returns
    assert!(matches!(t.state, State::Queued | State::Running), "{:?}", t.state);
    assert_eq!(files_under(&env.dl()), vec!["movie.mp4"], "no .part/.zdm leftovers");
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn same_file_name_gets_numbered() {
    let srv = testserver::start(server_cfg(MIB, "x.zip")).await;
    let env = Env::new(None);
    for p in ["/1", "/2", "/3"] {
        env.add(srv.url(p));
    }
    env.until("all complete", |ts| ts.len() == 3 && ts.iter().all(|t| t.state == State::Completed)).await;
    assert_eq!(files_under(&env.dl()), vec!["x (1).zip", "x (2).zip", "x.zip"]);
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn queue_limit_is_respected() {
    let srv = testserver::start(server_cfg(2 * MIB, "f.bin")).await;
    let env = Env::new(Some(2 * MIB)); // ~1 s per task
    env.m.queue_set(&QueueInfo { name: "Main".into(), max_parallel: 2, window: None, limit_kib: 0 }).unwrap();
    for i in 0..5 {
        env.add(srv.url(&format!("/{i}")));
    }
    let mut max_running = 0;
    let t = Instant::now();
    loop {
        let ts = env.m.list().unwrap();
        max_running = max_running.max(ts.iter().filter(|t| t.state == State::Running).count());
        if ts.iter().all(|t| t.state == State::Completed) {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(60), "{ts:#?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(max_running, 2);
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pause_keeps_progress_and_resume_finishes() {
    let size = 8 * MIB;
    let srv = testserver::start(server_cfg(size, "big.bin")).await;
    let env = Env::new(Some(2 * MIB)); // single download takes a few seconds
    let id = env.add(srv.url("/big")).id;
    env.until("some progress", |ts| ts[0].downloaded > 0).await;
    env.m.pause(id).unwrap();
    env.until("stopped", |ts| ts[0].state == State::Paused).await;
    tokio::time::sleep(Duration::from_millis(400)).await; // let the engine wind down
    let t = env.m.get(id).unwrap();
    assert_eq!(t.state, State::Paused);
    assert!(t.downloaded < size);
    let dest = t.dest.clone().unwrap();
    assert!(Path::new(&format!("{dest}.part.zdm")).exists(), "resume state must be kept");
    let before = srv.requests().len();
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(srv.requests().len(), before, "paused task must not keep requesting");

    env.m.resume(id).unwrap();
    env.until("completion", |ts| ts[0].state == State::Completed).await;
    assert_eq!(std::fs::read(dest).unwrap(), expected(size));
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn remove_with_delete_files_cleans_up_running_task() {
    let srv = testserver::start(server_cfg(8 * MIB, "gone.bin")).await;
    let env = Env::new(Some(2 * MIB));
    let id = env.add(srv.url("/g")).id;
    env.until("running with file", |ts| ts[0].state == State::Running && ts[0].dest.is_some()).await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    env.m.remove(id, true).await.unwrap();
    assert!(env.m.list().unwrap().is_empty());
    assert!(files_under(&env.dl()).is_empty(), "left behind: {:?}", files_under(&env.dl()));
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn time_window_holds_and_releases_tasks() {
    let srv = testserver::start(server_cfg(MIB, "w.bin")).await;
    let env = Env::new(None);
    // Only 22:00-23:00 on Wednesdays (weekday 2); the fake clock says Wednesday noon.
    env.m
        .queue_set(&QueueInfo { name: "Main".into(), max_parallel: 3, window: Some(Window { start_min: 22 * 60, end_min: 23 * 60, days: 1 << 2 }), limit_kib: 0 })
        .unwrap();
    env.add(srv.url("/w"));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(env.m.list().unwrap()[0].state, State::Queued);
    assert!(srv.requests().is_empty(), "nothing may be requested outside the window");

    *env.clock.lock().unwrap() = (22 * 60 + 30, 2);
    env.until("completion in window", |ts| ts[0].state == State::Completed).await;
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn closing_window_stops_running_task_then_resumes_it() {
    let size = 8 * MIB;
    let srv = testserver::start(server_cfg(size, "n.bin")).await;
    let env = Env::new(Some(2 * MIB));
    env.m
        .queue_set(&QueueInfo { name: "Main".into(), max_parallel: 3, window: Some(Window { start_min: 10 * 60, end_min: 14 * 60, days: 0x7f }), limit_kib: 0 })
        .unwrap();
    env.add(srv.url("/n"));
    env.until("running", |ts| ts[0].state == State::Running && ts[0].downloaded > 0).await;
    *env.clock.lock().unwrap() = (15 * 60, 2); // window closed
    env.until("back in line", |ts| ts[0].state == State::Queued).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(env.m.list().unwrap()[0].state, State::Queued);
    *env.clock.lock().unwrap() = (11 * 60, 2);
    let ts = env.until("completion", |ts| ts[0].state == State::Completed).await;
    assert_eq!(std::fs::read(ts[0].dest.clone().unwrap()).unwrap(), expected(size));
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_task_reports_error_and_can_be_retried() {
    let srv = testserver::start(ServerCfg { size: MIB, ranges: true, auth: Some("Basic nope".into()), ..Default::default() }).await;
    let env = Env::new(None);
    let id = env.add(srv.url("/p")).id;
    let ts = env.until("failure", |ts| ts[0].state == State::Failed).await;
    assert!(ts[0].error.as_deref().unwrap().contains("401"), "{:?}", ts[0].error);
    env.m.resume(id).unwrap();
    env.until("failure again", |ts| ts[0].state == State::Failed).await;
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn control_protocol_round_trip() {
    let srv = testserver::start(server_cfg(MIB, "p.pdf")).await;
    let env = Env::new(None);
    let (client, server) = tokio::io::duplex(64 * 1024);
    tokio::spawn(serve_conn(env.m.clone(), server));
    let mut c = BufReader::new(client);

    assert!(matches!(roundtrip(&mut c, &Request::Ping).await.unwrap(), Response::Pong { .. }));
    let added = roundtrip(&mut c, &Request::Add(AddRequest { url: srv.url("/p"), start: true, ..Default::default() })).await.unwrap();
    let Response::Task(t) = added else { panic!("{added:?}") };
    env.until("done", |ts| ts[0].state == State::Completed).await;
    let Response::Tasks { tasks } = roundtrip(&mut c, &Request::List).await.unwrap() else { panic!() };
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].id, t.id);
    assert_eq!(tasks[0].category.as_deref(), Some("Documents"));

    // Errors come back as responses, not dropped connections.
    let bad = roundtrip(&mut c, &Request::Add(AddRequest { url: "gopher://x/y".into(), ..Default::default() })).await.unwrap();
    assert!(matches!(bad, Response::Error { .. }));
    let missing = roundtrip(&mut c, &Request::Pause { id: 999 }).await.unwrap();
    assert!(matches!(missing, Response::Error { .. }));
    let bad_queue = roundtrip(&mut c, &Request::Add(AddRequest { url: srv.url("/q"), queue: Some("nope".into()), ..Default::default() })).await.unwrap();
    assert!(matches!(bad_queue, Response::Error { .. }));
    // The default queue is protected.
    let rm = roundtrip(&mut c, &Request::QueueRemove { name: "Main".into() }).await.unwrap();
    assert!(matches!(rm, Response::Error { .. }));
    env.stop().await;
}

#[test]
fn interrupted_tasks_are_requeued_on_restart() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.db");
    let id = {
        let s = Store::open(&db).unwrap();
        let id = s.insert_task(&NewTask { url: "http://x/y", filename: None, dest_dir: None, queue: "Main", state: State::Queued, referer: None, user_agent: None, headers: &[] }).unwrap();
        s.set_state(id, State::Running, None).unwrap();
        id
    };
    let s = Store::open(&db).unwrap();
    assert_eq!(s.task(id).unwrap().unwrap().info.state, State::Queued);
}

#[test]
fn headers_and_queues_persist() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.db");
    let hdrs = vec![("Cookie".to_string(), "a=b".to_string())];
    {
        let s = Store::open(&db).unwrap();
        s.insert_task(&NewTask { url: "http://x/y", filename: Some("n.bin"), dest_dir: None, queue: "Main", state: State::Paused, referer: Some("http://ref/"), user_agent: Some("UA/1"), headers: &hdrs }).unwrap();
        s.set_queue(&QueueInfo { name: "Night".into(), max_parallel: 2, window: Some(Window { start_min: 60, end_min: 360, days: 0x7f }), limit_kib: 0 }).unwrap();
    }
    let s = Store::open(&db).unwrap();
    let t = s.tasks().unwrap().remove(0);
    assert_eq!((t.headers, t.referer.as_deref(), t.info.state), (hdrs, Some("http://ref/"), State::Paused));
    let q = s.queue("Night").unwrap().unwrap();
    assert_eq!((q.max_parallel, q.window.unwrap().start_min), (2, 60));
}

#[tokio::test]
async fn settings_apply_persist_and_reject_bad_values() {
    let env = Env::new(None);
    let mut s = env.m.settings();
    assert_eq!(s.connections, 4);
    s.connections = 2;
    s.download_root = env.root.path().join("elsewhere").to_string_lossy().into_owned();
    env.m.set_settings(s.clone()).unwrap();
    assert_eq!(env.m.settings(), s);
    assert!(env.m.set_settings(zdm_proto::Settings { connections: 99, ..s.clone() }).is_err());
    assert_eq!(env.m.settings().connections, 2, "a rejected change must not apply");
    // a second manager on the same data dir sees the saved file
    let cfg = Config { db_path: env.root.path().join("unused.db"), download_root: env.dl(), connections: 4, limit_bps: None, tick: Duration::from_millis(50), retry_delay: Duration::from_millis(50), after_grace: Duration::from_millis(150) };
    let m2 = Manager::with_store(cfg, Store::open_in_memory().unwrap(), Arc::new(|| (0, 0)));
    assert_eq!(m2.settings(), s);
    // new downloads honour the new folder
    let srv = testserver::start(server_cfg(MIB, "a.zip")).await;
    env.add(srv.url("/a.zip"));
    let tasks = env.until("completed", |t| t.iter().all(|x| x.state == State::Completed)).await;
    assert!(tasks[0].dest.as_deref().unwrap().contains("elsewhere"), "{:?}", tasks[0].dest);
    env.stop().await;
}

#[tokio::test]
async fn move_task_between_queues() {
    let env = Env::new(None);
    env.m.queue_set(&QueueInfo { name: "Night".into(), max_parallel: 1, window: None, limit_kib: 0 }).unwrap();
    let t = env.m.add(AddRequest { url: "http://127.0.0.1:1/x.bin".into(), start: false, ..Default::default() }).unwrap();
    env.m.move_task(t.id, "Night").unwrap();
    assert_eq!(env.m.get(t.id).unwrap().queue, "Night");
    assert!(env.m.move_task(t.id, "Nope").is_err());
    env.stop().await;
}

#[tokio::test]
async fn grab_request_finds_files_through_the_protocol() {
    use zdm_core::testserver::{serve_site, Routes};
    let mut r = Routes::new();
    r.insert("/".into(), ("text/html".into(), br#"<a href="a.iso">a</a><a href="b.txt">b</a><a href="more.html">m</a>"#.to_vec()));
    r.insert("/more.html".into(), ("text/html".into(), br#"<a href="c.iso">c</a>"#.to_vec()));
    let site = serve_site(r).await;
    let env = Env::new(None);
    let links = env.m.grab(zdm_proto::GrabRequest { url: site.url("/"), depth: 1, same_host: true, extensions: vec![".ISO".into()], max_pages: 0, include: vec![], exclude: vec![] }).await.unwrap();
    assert_eq!(links.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(), ["a.iso", "c.iso"]);
    assert!(env.m.grab(zdm_proto::GrabRequest { url: "ftp://x".into(), depth: 0, same_host: true, extensions: vec![], max_pages: 0, include: vec![], exclude: vec![] }).await.is_err());
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn video_stream_url_downloads_through_the_manager() {
    use zdm_core::testserver::{serve_site, Routes};
    std::env::set_var("ZDM_FFMPEG", "none"); // keep the result a plain .ts so the content can be compared
    let seg = |i: usize| -> Vec<u8> { (0..3000).map(|n| ((n + i * 11) % 251) as u8).collect() };
    let mut r = Routes::new();
    r.insert("/show/ep1/index.m3u8".into(), ("application/vnd.apple.mpegurl".into(), b"#EXTM3U\n#EXTINF:4,\nc0.ts\n#EXTINF:4,\nc1.ts\n#EXTINF:4,\nc2.ts\n#EXT-X-ENDLIST\n".to_vec()));
    for i in 0..3 {
        r.insert(format!("/show/ep1/c{i}.ts"), ("video/mp2t".into(), seg(i)));
    }
    let site = serve_site(r).await;
    let env = Env::new(None);
    env.add(site.url("/show/ep1/index.m3u8"));
    let tasks = env.until("stream completion", |t| t.iter().all(|x| x.state == State::Completed || x.state == State::Failed)).await;
    let t = &tasks[0];
    assert_eq!(t.state, State::Completed, "{:?}", t.error);
    assert_eq!(Path::new(t.dest.as_deref().unwrap()), env.dl().join("Video").join("ep1.ts"));
    assert_eq!(std::fs::read(t.dest.as_deref().unwrap()).unwrap(), (0..3).flat_map(seg).collect::<Vec<u8>>());
    assert_eq!(t.size, Some(9000));
    assert_eq!(t.downloaded, 9000);
    // deleting with files removes the video and any work folder
    env.m.remove(t.id, true).await.unwrap();
    assert!(files_under(&env.dl()).is_empty(), "{:?}", files_under(&env.dl()));
    env.stop().await;
}

#[test]
fn network_errors_are_retried_but_final_ones_are_not() {
    use zdmd::manager::is_transient;
    assert!(is_transient("probe request failed: error sending request for url (http://x): connection refused"));
    assert!(is_transient("server returned 503 Service Unavailable for http://x"));
    assert!(is_transient("server returned 429 Too Many Requests for http://x"));
    assert!(is_transient("HTTP status client error (408 Request Timeout) for url (http://x)"));
    assert!(!is_transient("server returned 404 Not Found for http://x"));
    assert!(!is_transient("HTTP status client error (403 Forbidden) for url (http://x)"));
    assert!(!is_transient("this is a live stream (no end marker)"));
    assert!(!is_transient("bad proxy URL ftp://p"));
}

#[tokio::test(flavor = "multi_thread")]
async fn same_link_is_not_queued_twice_unless_asked() {
    let srv = testserver::start(server_cfg(4 * MIB, "a.zip")).await;
    let env = Env::new(None);
    let url = srv.url("/a.zip");
    let a = env.add(url.clone());
    let b = env.add(url.clone());
    assert_eq!(a.id, b.id, "the running task is returned again");
    assert_eq!(env.m.list().unwrap().len(), 1);
    env.until("completion", |t| t.iter().all(|x| x.state == State::Completed)).await;
    // finished downloads may be fetched again (it gets a numbered name); explicit duplicates are always allowed
    let c = env.add(url.clone());
    assert_ne!(c.id, a.id);
    let d = env.m.add(AddRequest { url, start: true, allow_duplicate: true, ..Default::default() }).unwrap();
    assert!(d.id != a.id && d.id != c.id);
    // a paused duplicate is resumed instead of being added again
    env.m.pause(d.id).ok();
    let p = env.m.add(AddRequest { url: "http://127.0.0.1:1/paused.bin".into(), start: false, ..Default::default() }).unwrap();
    env.m.pause(p.id).ok();
    let again = env.m.add(AddRequest { url: "http://127.0.0.1:1/paused.bin".into(), start: true, ..Default::default() }).unwrap();
    assert_eq!(again.id, p.id);
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn network_failure_is_retried_automatically_then_gives_up() {
    let env = Env::new(None);
    let mut s = env.m.settings();
    s.auto_retries = 2;
    env.m.set_settings(s).unwrap();
    let t = env.add("http://127.0.0.1:1/never.bin".into()); // nothing listens there: connection refused
    let tasks = env
        .until("final failure", |ts| ts.iter().all(|x| x.state == State::Failed && x.error.as_deref().is_some_and(|e| !e.starts_with("Will retry"))))
        .await;
    assert_eq!(env.m.retry_count(), 2, "two automatic retries, then it stays failed");
    assert!(tasks[0].error.is_some());
    // a manual resume starts the count again
    env.m.resume(t.id).unwrap();
    env.until("failed again", |ts| ts.iter().any(|x| x.error.as_deref().is_some_and(|e| e.starts_with("Will retry")) || x.state == State::Running || x.state == State::Queued)).await;
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn after_all_action_runs_once_when_everything_is_done() {
    let srv = testserver::start(server_cfg(2 * MIB, "a.zip")).await;
    let env = Env::new(None);
    let calls: Arc<Mutex<Vec<zdm_proto::AfterAll>>> = Arc::default();
    let c2 = calls.clone();
    env.m.set_executor(move |a| c2.lock().unwrap().push(a.clone()));

    // armed while idle with nothing to wait for: never fires
    env.m.set_after_all(zdm_proto::AfterAll::Shutdown).unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(calls.lock().unwrap().is_empty(), "nothing finished, nothing to do");

    // armed, then work happens: fires once after the grace period and disarms itself
    env.m.set_after_all(zdm_proto::AfterAll::Sleep).unwrap();
    env.add(srv.url("/a.zip"));
    env.until("completion", |t| t.iter().all(|x| x.state == State::Completed)).await;
    let t0 = Instant::now();
    while calls.lock().unwrap().is_empty() && t0.elapsed() < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(*calls.lock().unwrap(), vec![zdm_proto::AfterAll::Sleep]);
    assert_eq!(env.m.after_all(), zdm_proto::AfterAll::Nothing, "one shot");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(calls.lock().unwrap().len(), 1);

    // cancelling before it fires
    env.m.set_after_all(zdm_proto::AfterAll::Shutdown).unwrap();
    env.m.set_after_all(zdm_proto::AfterAll::Nothing).unwrap();
    env.add(srv.url("/b.zip"));
    env.until("second completion", |t| t.iter().all(|x| x.state == State::Completed)).await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(calls.lock().unwrap().len(), 1, "a cancelled action must not run");
    assert!(env.m.set_after_all(zdm_proto::AfterAll::Command { command: " ".into() }).is_err());
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn category_folder_overrides_and_post_command() {
    let srv = testserver::start(server_cfg(MIB, "clip.mp4")).await;
    let env = Env::new(None);
    let movies = env.root.path().join("my movies");
    let mut s = env.m.settings();
    s.category_dirs.insert("Video".into(), movies.to_string_lossy().into_owned());
    // the file is available as $ZDM_FILE (not pasted into the command): record its size next to it
    s.post_command = Some("wc -c < \"$ZDM_FILE\" > \"$ZDM_FILE.scan\"".into());
    env.m.set_settings(s).unwrap();
    env.add(srv.url("/v"));
    let tasks = env.until("completion", |t| t.iter().all(|x| x.state == State::Completed)).await;
    let dest = tasks[0].dest.clone().unwrap();
    assert_eq!(Path::new(&dest), movies.join("clip.mp4"));
    let scan = format!("{dest}.scan");
    let t0 = Instant::now();
    while !Path::new(&scan).exists() && t0.elapsed() < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(std::fs::read_to_string(&scan).unwrap().trim(), MIB.to_string());

    // a failing command is noted on the task, which stays completed with its file
    let mut s = env.m.settings();
    s.post_command = Some("exit 3".into());
    env.m.set_settings(s).unwrap();
    env.add(srv.url("/w"));
    let t0 = Instant::now();
    let tasks = loop {
        let ts = env.m.list().unwrap();
        if ts.iter().any(|x| x.error.as_deref().is_some_and(|e| e.contains("exited with 3"))) || t0.elapsed() > Duration::from_secs(8) {
            break ts;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let bad = tasks.iter().find(|x| x.error.is_some()).expect("failure note");
    assert_eq!(bad.state, State::Completed);
    assert!(Path::new(bad.dest.as_deref().unwrap()).exists());
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn global_limit_applies_to_all_downloads_and_can_be_changed() {
    let srv = testserver::start(server_cfg(MIB, "x.bin")).await;
    let env = Env::new(None);
    let mut s = env.m.settings();
    s.global_limit_kib = 512; // 0.5 MiB/s for everything
    env.m.set_settings(s.clone()).unwrap();
    let t0 = Instant::now();
    env.add(srv.url("/1"));
    env.add(srv.url("/2"));
    env.until("both done", |t| t.len() == 2 && t.iter().all(|x| x.state == State::Completed)).await;
    assert!(t0.elapsed() >= Duration::from_millis(2500), "2 MiB at 0.5 MiB/s needs ~4 s, took {:?}", t0.elapsed());
    s.global_limit_kib = 0;
    env.m.set_settings(s).unwrap();
    let t1 = Instant::now();
    env.add(srv.url("/3"));
    env.until("third done", |t| t.len() == 3 && t.iter().all(|x| x.state == State::Completed)).await;
    assert!(t1.elapsed() < Duration::from_secs(2), "unlimited again: {:?}", t1.elapsed());
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn logins_work_for_digest_and_basic_and_stay_out_of_the_task_list() {
    use base64::Engine;
    let digest = testserver::start(ServerCfg { digest: Some(("alice".into(), "s3cret:x".into())), ..server_cfg(2 * MIB, "secure.zip") }).await;
    let basic = testserver::start(ServerCfg { auth: Some(format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("bob:pw"))), ..server_cfg(MIB, "plain.zip") }).await;
    let env = Env::new(None);

    // credentials inside the link (the password even contains a colon)
    let link = digest.url("/secure.zip").replacen("http://", "http://alice:s3cret%3Ax@", 1);
    let t = env.m.add(AddRequest { url: link, start: true, ..Default::default() }).unwrap();
    assert!(!t.url.contains("s3cret") && !t.url.contains('@'), "password leaked into the task: {}", t.url);
    // credentials passed separately
    env.m.add(AddRequest { url: basic.url("/plain.zip"), start: true, auth: Some(("bob".into(), "pw".into())), ..Default::default() }).unwrap();
    // wrong password: fails, and is not retried forever (401 is final)
    let mut s = env.m.settings();
    s.auto_retries = 0;
    env.m.set_settings(s).unwrap();
    env.m.add(AddRequest { url: basic.url("/other.zip"), start: true, auth: Some(("bob".into(), "nope".into())), ..Default::default() }).unwrap();

    let tasks = env.until("outcomes", |ts| ts.len() == 3 && ts.iter().all(|x| matches!(x.state, State::Completed | State::Failed))).await;
    let by = |n: &str| tasks.iter().find(|x| x.url.ends_with(n)).unwrap().clone();
    assert_eq!(by("secure.zip").state, State::Completed, "{:?}", by("secure.zip").error);
    assert_eq!(by("plain.zip").state, State::Completed, "{:?}", by("plain.zip").error);
    assert_eq!(by("other.zip").state, State::Failed);
    let json = serde_json::to_string(&tasks).unwrap();
    assert!(!json.contains("s3cret") && !json.contains("nope"), "credentials must not appear in any answer: {json}");
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ftp_links_download_with_and_without_login() {
    let data = zdm_core::testserver::expected(700_000);
    let open = testserver::start_ftp(std::collections::HashMap::from([("/pub/bundle.zip".to_string(), data.clone())]), None, true).await;
    let locked = testserver::start_ftp(std::collections::HashMap::from([("/priv/notes.pdf".to_string(), data[..1234].to_vec())]), Some(("dave".into(), "hunter2".into())), true).await;
    let env = Env::new(None);
    env.add(open.url("/pub/bundle.zip"));
    let link = locked.url("/priv/notes.pdf").replacen("ftp://", "ftp://dave:hunter2@", 1);
    let t = env.m.add(AddRequest { url: link, start: true, ..Default::default() }).unwrap();
    assert!(!t.url.contains("hunter2"), "{}", t.url);
    let tasks = env.until("ftp done", |ts| ts.len() == 2 && ts.iter().all(|x| matches!(x.state, State::Completed | State::Failed))).await;
    for x in &tasks {
        assert_eq!(x.state, State::Completed, "{:?} {:?}", x.url, x.error);
    }
    let zip = tasks.iter().find(|x| x.url.ends_with("bundle.zip")).unwrap();
    assert_eq!(Path::new(zip.dest.as_deref().unwrap()), env.dl().join("Compressed").join("bundle.zip"));
    assert_eq!(std::fs::read(zip.dest.as_deref().unwrap()).unwrap(), data);
    assert_eq!((zip.size, zip.downloaded), (Some(700_000), 700_000));
    let pdf = tasks.iter().find(|x| x.url.ends_with("notes.pdf")).unwrap();
    assert_eq!(std::fs::read(pdf.dest.as_deref().unwrap()).unwrap(), data[..1234]);
    assert!(!serde_json::to_string(&tasks).unwrap().contains("hunter2"));
    // unsupported schemes are still refused
    assert!(env.m.add(AddRequest { url: "gopher://x/y".into(), start: true, ..Default::default() }).is_err());
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn new_address_for_a_paused_download_keeps_its_progress() {
    let size = 8 * MIB;
    let a = testserver::start(server_cfg(size, "big.bin")).await;
    let b = testserver::start(server_cfg(size, "big.bin")).await; // a mirror: same size, same validator
    let env = Env::new(Some(2 * MIB));
    let id = env.add(a.url("/big")).id;
    env.until("some progress", |ts| ts[0].downloaded > MIB / 2).await;
    env.m.pause(id).unwrap();
    env.until("stopped", |ts| ts[0].state == State::Paused).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(env.m.set_url(id, "gopher://nope").is_err());
    env.m.set_url(id, &b.url("/big")).unwrap();
    assert!(env.m.get(id).unwrap().url.starts_with(&b.url("")));
    env.m.resume(id).unwrap();
    env.until("completion", |ts| ts[0].state == State::Completed).await;
    let t = env.m.get(id).unwrap();
    assert_eq!(std::fs::read(t.dest.unwrap()).unwrap(), expected(size));
    let asked: u64 = b
        .requests()
        .iter()
        .filter_map(|r| r.range.as_deref())
        .filter(|r| *r != "bytes=0-0")
        .filter_map(|r| {
            let (x, y) = r.strip_prefix("bytes=")?.split_once('-')?;
            Some(y.parse::<u64>().ok()? - x.parse::<u64>().ok()? + 1)
        })
        .sum();
    assert!(asked > 0 && asked < size - MIB / 4, "the mirror must only be asked for the missing part: {asked} of {size}");
    // a running or finished task cannot be re-addressed
    assert!(env.m.set_url(id, &b.url("/big")).is_err());
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_download_gets_a_new_address_and_restarts() {
    let srv = testserver::start(server_cfg(MIB, "ok.bin")).await;
    let env = Env::new(None);
    let mut s = env.m.settings();
    s.auto_retries = 0;
    env.m.set_settings(s).unwrap();
    let id = env.add("http://127.0.0.1:1/dead.bin".into()).id;
    env.until("failure", |ts| ts[0].state == State::Failed).await;
    env.m.set_url(id, &srv.url("/ok.bin")).unwrap();
    env.until("completion", |ts| ts[0].state == State::Completed).await;
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn move_to_top_changes_the_start_order() {
    let srv = testserver::start(server_cfg(MIB, "q.bin")).await;
    let env = Env::new(Some(MIB)); // each takes about a second
    env.m.queue_set(&QueueInfo { name: "One".into(), max_parallel: 1, window: None, limit_kib: 0 }).unwrap();
    let add = |p: &str| env.m.add(AddRequest { url: srv.url(p), queue: Some("One".into()), start: true, ..Default::default() }).unwrap().id;
    let (first, second, third) = (add("/1"), add("/2"), add("/3"));
    env.until("first running", |ts| ts.iter().any(|t| t.id == first && t.state == State::Running)).await;
    env.m.move_to_top(third).unwrap();
    let mut order = vec![];
    let t0 = Instant::now();
    while order.len() < 3 && t0.elapsed() < Duration::from_secs(20) {
        for t in env.m.list().unwrap() {
            if t.state == State::Completed && !order.contains(&t.id) {
                order.push(t.id);
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(order, vec![first, third, second]);
    assert!(env.m.move_to_top(first).is_err(), "finished tasks cannot be moved");
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn existing_file_names_follow_the_chosen_policy() {
    let env = Env::new(None);
    let get = |size: u64| async move { testserver::start(server_cfg(size, "same.bin")).await };
    let (s1, s2, s3) = (get(MIB).await, get(2 * MIB).await, get(3 * MIB).await);

    env.add(s1.url("/a"));
    env.until("first", |t| t.len() == 1 && t[0].state == State::Completed).await;
    let first = env.m.list().unwrap()[0].dest.clone().unwrap();

    // overwrite: same name, new content
    let mut s = env.m.settings();
    s.on_exists = "overwrite".into();
    env.m.set_settings(s.clone()).unwrap();
    env.add(s2.url("/b"));
    env.until("second", |t| t.len() == 2 && t.iter().all(|x| x.state == State::Completed)).await;
    let second = env.m.list().unwrap()[1].dest.clone().unwrap();
    assert_eq!(first, second, "overwrite reuses the name");
    assert_eq!(std::fs::read(&second).unwrap(), expected(2 * MIB));

    // skip: refused, and not retried
    s.on_exists = "skip".into();
    env.m.set_settings(s.clone()).unwrap();
    env.add(s3.url("/c"));
    env.until("skipped", |t| t.len() == 3 && t[2].state == State::Failed).await;
    let t = env.m.list().unwrap()[2].clone();
    assert!(t.error.as_deref().unwrap().contains("already exists"), "{:?}", t.error);
    assert_eq!(env.m.retry_count(), 0);
    assert_eq!(std::fs::read(&first).unwrap(), expected(2 * MIB), "the existing file is untouched");

    // rename (the default): a numbered name
    s.on_exists = "rename".into();
    env.m.set_settings(s).unwrap();
    env.m.resume(t.id).unwrap();
    env.until("renamed", |t| t[2].state == State::Completed).await;
    assert!(env.m.list().unwrap()[2].dest.clone().unwrap().ends_with("same (1).bin"));
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_queue_can_have_its_own_speed_limit() {
    let srv = testserver::start(server_cfg(MIB, "x.bin")).await;
    let env = Env::new(None);
    env.m.queue_set(&QueueInfo { name: "Main".into(), max_parallel: 3, window: None, limit_kib: 256 }).unwrap();
    assert_eq!(env.m.queue_list().unwrap().iter().find(|q| q.name == "Main").unwrap().limit_kib, 256);
    let t0 = Instant::now();
    env.add(srv.url("/1"));
    env.until("done", |t| t.len() == 1 && t[0].state == State::Completed).await;
    assert!(t0.elapsed() >= Duration::from_millis(2500), "1 MiB at 256 KiB/s needs ~4 s, took {:?}", t0.elapsed());
    env.m.queue_set(&QueueInfo { name: "Main".into(), max_parallel: 3, window: None, limit_kib: 0 }).unwrap();
    let t1 = Instant::now();
    env.add(srv.url("/2"));
    env.until("done again", |t| t.len() == 2 && t.iter().all(|x| x.state == State::Completed)).await;
    assert!(t1.elapsed() < Duration::from_millis(2000), "{:?}", t1.elapsed());
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn custom_file_types_decide_the_category_folder() {
    let srv = testserver::start(server_cfg(MIB / 4, "notes.bin")).await;
    let env = Env::new(None);
    let mut s = env.m.settings();
    s.category_exts.insert("Documents".into(), vec!["bin".into(), "txt".into()]);
    env.m.set_settings(s).unwrap();
    env.add(srv.url("/n"));
    let tasks = env.until("done", |t| t.len() == 1 && t[0].state == State::Completed).await;
    assert_eq!(tasks[0].category.as_deref(), Some("Documents"));
    assert_eq!(Path::new(tasks[0].dest.as_deref().unwrap()), env.dl().join("Documents").join("notes.bin"));
    env.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn saved_site_logins_are_used_without_typing_them_again() {
    use base64::Engine;
    let basic = testserver::start(ServerCfg { auth: Some(format!("Basic {}", base64::engine::general_purpose::STANDARD.encode("kim:pw1"))), ..server_cfg(MIB, "locked.zip") }).await;
    let digest = testserver::start(ServerCfg { digest: Some(("lee".into(), "pw2".into())), ..server_cfg(MIB, "safe.zip") }).await;
    let env = Env::new(None);
    let mut s = env.m.settings();
    s.auto_retries = 0;
    // both test servers are on 127.0.0.1: the login of the exact host wins over a parent domain, and a wrong one stays wrong
    s.site_logins = vec![
        zdm_proto::SiteLogin { host: "127.0.0.1".into(), user: "kim".into(), pass: "pw1".into() },
        zdm_proto::SiteLogin { host: "0.0.1".into(), user: "other".into(), pass: "nope".into() },
    ];
    env.m.set_settings(s.clone()).unwrap();
    assert_eq!(env.m.login_for(&basic.url("/x")), Some(("kim".into(), "pw1".into())));
    assert_eq!(env.m.login_for("https://elsewhere.test/x"), None);
    let sub = zdm_proto::SiteLogin { host: "example.com".into(), user: "u".into(), pass: "p".into() };
    s.site_logins.push(sub);
    env.m.set_settings(s.clone()).unwrap();
    assert_eq!(env.m.login_for("https://files.example.com/a.zip"), Some(("u".into(), "p".into())), "subdomains use the parent's login");
    assert_eq!(env.m.login_for("https://notexample.com/a.zip"), None, "but a different site that merely ends alike does not");

    let a = env.add(basic.url("/locked.zip"));
    let a = env.until("basic done", |t| t.iter().any(|x| x.id == a.id && matches!(x.state, State::Completed | State::Failed))).await.into_iter().find(|x| x.id == a.id).unwrap();
    assert_eq!(a.state, State::Completed, "{:?}", a.error);
    // a login typed with the task beats the saved one; the saved one is wrong for the digest server
    let d = env.m.add(AddRequest { url: digest.url("/safe.zip"), start: true, auth: Some(("lee".into(), "pw2".into())), ..Default::default() }).unwrap();
    let all = env.until("digest done", |t| t.iter().any(|x| x.id == d.id && matches!(x.state, State::Completed | State::Failed))).await;
    assert_eq!(all.iter().find(|x| x.id == d.id).unwrap().state, State::Completed);
    env.stop().await;
}
