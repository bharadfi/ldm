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
    env.m.queue_set(&QueueInfo { name: "Main".into(), max_parallel: 2, window: None }).unwrap();
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
        .queue_set(&QueueInfo { name: "Main".into(), max_parallel: 3, window: Some(Window { start_min: 22 * 60, end_min: 23 * 60, days: 1 << 2 }) })
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
        .queue_set(&QueueInfo { name: "Main".into(), max_parallel: 3, window: Some(Window { start_min: 10 * 60, end_min: 14 * 60, days: 0x7f }) })
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
    let bad = roundtrip(&mut c, &Request::Add(AddRequest { url: "ftp://x/y".into(), ..Default::default() })).await.unwrap();
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
        s.set_queue(&QueueInfo { name: "Night".into(), max_parallel: 2, window: Some(Window { start_min: 60, end_min: 360, days: 0x7f }) }).unwrap();
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
    let cfg = Config { db_path: env.root.path().join("unused.db"), download_root: env.dl(), connections: 4, limit_bps: None, tick: Duration::from_millis(50) };
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
    env.m.queue_set(&QueueInfo { name: "Night".into(), max_parallel: 1, window: None }).unwrap();
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
    let links = env.m.grab(zdm_proto::GrabRequest { url: site.url("/"), depth: 1, same_host: true, extensions: vec![".ISO".into()], max_pages: 0 }).await.unwrap();
    assert_eq!(links.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(), ["a.iso", "c.iso"]);
    assert!(env.m.grab(zdm_proto::GrabRequest { url: "ftp://x".into(), depth: 0, same_host: true, extensions: vec![], max_pages: 0 }).await.is_err());
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
