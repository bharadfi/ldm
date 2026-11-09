//! Task manager: owns the store, runs downloads, enforces queue limits and time windows.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zdm_core::{build_client, download_probed, Cancelled, Limiter, Options};
use zdm_proto::{AddRequest, AfterAll, GrabLink, GrabRequest, PreviewInfo, QueueInfo, Settings, State, TaskInfo, ASK_MARKER};

use crate::categories::categorize_with;
use crate::config::Config;
use crate::store::{NewTask, Store, DEFAULT_QUEUE};

struct Running {
    gen: u64,
    queue: String,
    cancel: CancellationToken,
    handle: JoinHandle<()>,
}

/// Local (minute of day, weekday 0 = Monday).
pub type Clock = Arc<dyn Fn() -> (u32, u32) + Send + Sync>;

fn local_clock() -> (u32, u32) {
    use chrono::{Datelike, Local, Timelike};
    let n = Local::now();
    (n.hour() * 60 + n.minute(), n.weekday().num_days_from_monday())
}

fn settings_path(cfg: &Config) -> PathBuf {
    cfg.db_path.parent().map(Path::to_path_buf).unwrap_or_default().join("settings.json")
}

/// Saved settings if valid, else the defaults derived from the startup config.
fn load_settings(cfg: &Config) -> Settings {
    let defaults = Settings {
        connections: cfg.connections as u32,
        speed_limit_kib: cfg.limit_bps.map_or(0, |b| b / 1024),
        proxy: None,
        download_root: cfg.download_root.to_string_lossy().into_owned(),
        global_limit_kib: 0,
        category_dirs: Default::default(),
        auto_retries: 8,
        post_command: None,
        on_exists: "rename".into(),
        category_exts: Default::default(),
        site_logins: vec![],
        ytdlp_cookies_browser: None,
    };
    std::fs::read(settings_path(cfg)).ok().and_then(|b| serde_json::from_slice::<Settings>(&b).ok()).filter(|s| s.validate().is_ok()).unwrap_or(defaults)
}

fn save_settings(path: &Path, s: &Settings) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(s)?)?; // the proxy URL may carry a password
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(tmp, path)?;
    Ok(())
}

/// File-name stem for a stream URL: `/show/ep1/index.m3u8` -> `ep1` (generic playlist names use the folder).
fn hls_stem(url: &str) -> String {
    let u = url::Url::parse(url).ok();
    let segs: Vec<String> = u.as_ref().and_then(|u| u.path_segments()).map(|s| s.filter(|x| !x.is_empty()).map(str::to_string).collect()).unwrap_or_default();
    let stem = |s: &str| s.rsplit_once('.').map_or(s, |(a, _)| a).to_string();
    let generic = ["index", "playlist", "master", "chunklist", "stream", "manifest", "main"];
    let last = segs.last().map(|s| stem(s)).unwrap_or_default();
    let name = if (last.is_empty() || generic.contains(&last.to_ascii_lowercase().as_str())) && segs.len() > 1 { segs[segs.len() - 2].clone() } else { last };
    let name: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ' ') { c } else { '_' }).collect();
    if name.is_empty() { u.and_then(|u| u.host_str().map(str::to_string)).unwrap_or_else(|| "video".into()) } else { name }
}

/// Header name under which a task's credentials are kept with its other (stored, private) request headers.
const AUTH_HEADER: &str = "x-zdm-userinfo";
/// Header name under which a video-page task keeps what to fetch with yt-dlp.
const YTDLP_HEADER: &str = "x-zdm-ytdlp";

/// `http://user:pass@host/x` -> (`http://host/x`, Some((user, pass))). Any other string comes back unchanged.
pub fn split_userinfo(url: &str) -> (String, Option<(String, String)>) {
    let Ok(mut u) = url::Url::parse(url) else { return (url.to_string(), None) };
    if u.username().is_empty() && u.password().is_none() {
        return (url.to_string(), None);
    }
    let auth = (zdm_core::grab::percent_decode(u.username()), zdm_core::grab::percent_decode(u.password().unwrap_or("")));
    let _ = u.set_username("");
    let _ = u.set_password(None);
    (u.to_string(), Some(auth))
}

/// Starts with this text so the scheduler can tell a pending automatic retry from a final failure.
const RETRY_PREFIX: &str = "Will retry (attempt";

/// Network trouble that may go away (worth retrying) versus errors that will not fix themselves.
pub fn is_transient(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    const FINAL: [&str; 21] = ["proof that this is not a robot", "video unavailable", "is private", "sign in", "unsupported url", "requested format", "needs yt-dlp", "unsupported format", "no downloadable", "needs ffmpeg", "already exists", "only http", "bad proxy", "bad header", "bad credentials", "no such", "live stream", "unsupported", "could not decrypt", "not a web page", "is not a valid"];
    if FINAL.iter().any(|k| m.contains(k)) {
        return false;
    }
    // "server returned 404 Not Found for ..." / "HTTP status client error (404 Not Found) ..."
    let code = |after: &str| m.find(after).and_then(|i| m[i + after.len()..].split(|c: char| !c.is_ascii_digit()).next()?.parse::<u16>().ok());
    if let Some(c) = code("server returned ").or_else(|| code("client error (")) {
        return c == 408 || c == 429 || c >= 500;
    }
    true
}

/// Carries out an "after all downloads" action on the real system.
pub fn system_action(a: &AfterAll) {
    use std::process::Command;
    let r = match a {
        AfterAll::Nothing => return,
        AfterAll::Sleep => Command::new("systemctl").arg("suspend").status(),
        AfterAll::Shutdown => Command::new("systemctl").arg("poweroff").status(),
        AfterAll::Command { command } => Command::new("sh").arg("-c").arg(command).status(),
    };
    if let Err(e) = r {
        eprintln!("after-all action {a:?} failed: {e}");
    }
}

type Executor = Arc<dyn Fn(&AfterAll) + Send + Sync>;

#[derive(Default)]
struct AfterState {
    action: AfterAll,
    /// Something finished (or was still pending) since the action was armed.
    armed_with_work: bool,
    idle_since: Option<std::time::Instant>,
}

pub struct Manager {
    cfg: Config,
    global: Arc<Limiter>,
    queue_limits: Mutex<HashMap<String, Arc<Limiter>>>,
    retries: Mutex<HashMap<i64, u32>>,
    retry_total: AtomicU64,
    after: Mutex<AfterState>,
    executor: Mutex<Executor>,
    settings: Mutex<Settings>,
    store: Mutex<Store>,
    running: Mutex<HashMap<i64, Running>>,
    gen: AtomicU64,
    wake: Notify,
    clock: Clock,
}

impl Manager {
    pub fn open(cfg: Config) -> Result<Arc<Self>> {
        let store = Store::open(&cfg.db_path)?;
        Ok(Self::with_store(cfg, store, Arc::new(local_clock)))
    }

    pub fn with_store(cfg: Config, store: Store, clock: Clock) -> Arc<Self> {
        let settings = load_settings(&cfg);
        let global = Arc::new(Limiter::new(settings.global_limit_kib * 1024));
        Arc::new(Self { cfg, global, queue_limits: Mutex::default(), retries: Mutex::default(), retry_total: AtomicU64::new(0), after: Mutex::default(), executor: Mutex::new(Arc::new(system_action)), settings: Mutex::new(settings), store: Mutex::new(store), running: Mutex::new(HashMap::new()), gen: AtomicU64::new(1), wake: Notify::new(), clock })
    }

    pub fn settings(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }

    /// Validate, apply to downloads started from now on, and save next to the database.
    pub fn set_settings(&self, s: Settings) -> Result<()> {
        s.validate().map_err(|e| anyhow!(e))?;
        save_settings(&settings_path(&self.cfg), &s)?;
        self.global.set_rate(s.global_limit_kib * 1024);
        *self.settings.lock().unwrap() = s;
        Ok(())
    }

    /// The saved sign-in for the site of `url`, if any (an exact host match wins over a parent domain).
    pub fn login_for(&self, url: &str) -> Option<(String, String)> {
        let host = url::Url::parse(url).ok()?.host_str()?.to_ascii_lowercase();
        let set = self.settings.lock().unwrap();
        set.site_logins
            .iter()
            .filter(|l| {
                let h = l.host.to_ascii_lowercase();
                host == h || host.ends_with(&format!(".{h}"))
            })
            .max_by_key(|l| l.host.len())
            .map(|l| (l.user.clone(), l.pass.clone()))
    }

    /// What a video page offers, through yt-dlp.
    pub async fn formats(&self, url: &str, cookie: Option<&str>) -> Result<zdm_proto::FormatList> {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            bail!("only http:// and https:// pages can be looked up");
        }
        let s = self.settings();
        crate::ytdlp::list_formats(url, s.proxy.as_deref(), cookie, s.ytdlp_cookies_browser.as_deref()).await
    }

    /// Look for downloadable files on a page. Uses the proxy from the settings.
    pub async fn grab(&self, g: GrabRequest) -> Result<Vec<GrabLink>> {
        let opts = Options { proxy: self.settings().proxy, ..Options::default() };
        let client = build_client(&opts)?;
        let go = zdm_core::grab::GrabOptions {
            depth: g.depth.min(5),
            same_host: g.same_host,
            extensions: g.extensions.iter().map(|e| e.trim_start_matches('.').to_ascii_lowercase()).filter(|e| !e.is_empty()).collect(),
            max_pages: if g.max_pages == 0 { 100 } else { (g.max_pages as usize).min(300) },
            include: g.include.iter().map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect(),
            exclude: g.exclude.iter().map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect(),
            ..Default::default()
        };
        let cancel = CancellationToken::new();
        let found = tokio::time::timeout(Duration::from_secs(120), zdm_core::grab::crawl(&client, &g.url, &go, &cancel)).await.map_err(|_| anyhow!("the site took too long to explore"))??;
        Ok(found.into_iter().map(|f| GrabLink { url: f.url, name: f.name, ext: f.ext, from: f.from }).collect())
    }

    /// The limiters that apply to a download of `queue`: the global one and, when set, the queue's own.
    fn limiters_for(&self, queue: &str) -> Vec<Arc<Limiter>> {
        let mut v = vec![self.global.clone()];
        let kib = self.store().queue(queue).ok().flatten().map_or(0, |q| q.limit_kib);
        let mut map = self.queue_limits.lock().unwrap();
        let l = map.entry(queue.to_string()).or_insert_with(|| Arc::new(Limiter::new(0))).clone();
        l.set_rate(kib * 1024);
        v.push(l);
        v
    }

    /// Total automatic retries scheduled so far (for tests and diagnostics).
    pub fn retry_count(&self) -> u64 {
        self.retry_total.load(Ordering::Relaxed)
    }

    pub fn after_all(&self) -> AfterAll {
        self.after.lock().unwrap().action.clone()
    }

    /// Arm (or with `Nothing`, cancel) the one-shot action for when all downloads are done.
    pub fn set_after_all(&self, a: AfterAll) -> Result<()> {
        a.validate().map_err(|e| anyhow!(e))?;
        let busy = self.has_active_work();
        *self.after.lock().unwrap() = AfterState { action: a, armed_with_work: busy, idle_since: None };
        Ok(())
    }

    /// Replace what carries out the after-all action (tests use this so nothing really shuts down).
    pub fn set_executor(&self, f: impl Fn(&AfterAll) + Send + Sync + 'static) {
        *self.executor.lock().unwrap() = Arc::new(f);
    }

    fn has_active_work(&self) -> bool {
        if !self.running.lock().unwrap().is_empty() {
            return true;
        }
        self.store().tasks().map(|t| t.iter().any(|r| matches!(r.info.state, State::Running | State::Queued))).unwrap_or(true)
    }

    fn check_after_all(&self) {
        if self.after.lock().unwrap().action == AfterAll::Nothing {
            return;
        }
        let busy = self.has_active_work();
        let mut a = self.after.lock().unwrap();
        if busy {
            a.armed_with_work = true;
            a.idle_since = None;
            return;
        }
        if !a.armed_with_work {
            return;
        }
        match a.idle_since {
            None => a.idle_since = Some(std::time::Instant::now()),
            Some(t) if t.elapsed() >= self.cfg.after_grace => {
                let action = std::mem::take(&mut a.action);
                *a = AfterState::default();
                drop(a);
                let run = self.executor.lock().unwrap().clone();
                std::thread::spawn(move || run(&action));
            }
            Some(_) => {}
        }
    }

    /// Start this queued task before the others of its queue.
    pub fn move_to_top(&self, id: i64) -> Result<()> {
        if self.get(id)?.state == State::Completed {
            bail!("task {id} is already completed");
        }
        self.store().move_to_top(id)?;
        self.wake.notify_one();
        Ok(())
    }

    /// New address for a task that is not running. Progress is kept when the server's file is the same
    /// (the engine compares size and validators); a failed task is queued again.
    pub fn set_url(&self, id: i64, url: &str) -> Result<()> {
        let info = self.get(id)?;
        match info.state {
            State::Running => bail!("pause task {id} before changing its address"),
            State::Completed => bail!("task {id} is already completed"),
            _ => {}
        }
        if !(url.starts_with("http://") || url.starts_with("https://") || zdm_core::ftp::is_ftp_url(url)) {
            bail!("only http://, https:// and ftp:// addresses are supported");
        }
        let (clean, creds) = split_userinfo(url);
        let mut headers = self.store().task(id)?.map(|t| t.headers).unwrap_or_default();
        if let Some((u, p)) = creds {
            headers.retain(|(k, _)| !k.eq_ignore_ascii_case(AUTH_HEADER));
            headers.push((AUTH_HEADER.into(), format!("{}:{}", u.replace(':', "%3A"), p)));
        }
        self.store().set_url(id, &clean, &headers)?;
        if info.state == State::Failed {
            self.resume(id)?;
        }
        Ok(())
    }

    pub fn move_task(&self, id: i64, queue: &str) -> Result<()> {
        let st = self.get(id)?.state;
        if st == State::Running {
            bail!("pause task {id} before moving it to another queue");
        }
        if self.store().queue(queue)?.is_none() {
            bail!("no such queue: {queue}");
        }
        self.store().set_task_queue(id, queue)?;
        self.wake.notify_one();
        Ok(())
    }

    fn store(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store.lock().unwrap()
    }

    // ---- API ----

    pub fn add(&self, mut a: AddRequest) -> Result<TaskInfo> {
        if !(a.url.starts_with("http://") || a.url.starts_with("https://") || zdm_core::ftp::is_ftp_url(&a.url)) {
            bail!("only http://, https:// and ftp:// addresses are supported");
        }
        // credentials never stay in the visible link
        let (clean, from_url) = split_userinfo(&a.url);
        a.url = clean;
        if let Some(sel) = a.ytdlp.take() {
            if !crate::ytdlp::valid_selector(&sel) {
                bail!("unsupported format selection");
            }
            a.headers.retain(|(k, _)| !k.eq_ignore_ascii_case(YTDLP_HEADER));
            a.headers.push((YTDLP_HEADER.into(), sel));
        }
        if let Some((u, p)) = a.auth.take().or(from_url) {
            a.headers.retain(|(k, _)| !k.eq_ignore_ascii_case(AUTH_HEADER));
            a.headers.push((AUTH_HEADER.into(), format!("{}:{}", u.replace(':', "%3A"), p)));
        }
        if let Some(f) = &a.filename {
            if f.is_empty() || f.contains(['/', '\\']) || f == "." || f == ".." {
                bail!("filename must be a plain file name");
            }
        }
        if !a.allow_duplicate {
            // the same page with another format (1080p and the audio, say) is a different download
            let sel = |hs: &[(String, String)]| hs.iter().find(|(k, _)| k.eq_ignore_ascii_case(YTDLP_HEADER)).map(|(_, v)| v.clone());
            let wanted = sel(&a.headers);
            let existing = self.store().tasks()?.into_iter().find(|t| t.info.url == a.url && t.info.state != State::Completed && sel(&t.headers) == wanted);
            if let Some(t) = existing {
                if a.start && matches!(t.info.state, State::Paused | State::Failed) {
                    self.resume(t.info.id)?;
                }
                return self.get(t.info.id);
            }
        }
        let queue = a.queue.clone().unwrap_or_else(|| DEFAULT_QUEUE.to_string());
        let id = {
            let s = self.store();
            if s.queue(&queue)?.is_none() {
                bail!("no such queue: {queue}");
            }
            let state = if a.start { State::Queued } else { State::Paused };
            s.insert_task(&NewTask {
                url: &a.url,
                filename: a.filename.as_deref(),
                dest_dir: a.dest_dir.as_deref(),
                queue: &queue,
                state,
                referer: a.referer.as_deref(),
                user_agent: a.user_agent.as_deref(),
                headers: &a.headers,
            })?
        };
        if a.ask {
            self.store().set_state(id, State::Paused, Some(ASK_MARKER))?;
        }
        self.wake.notify_one();
        self.get(id)
    }

    /// Name, size and destination of a task that has not started, for the confirmation window.
    pub async fn preview(&self, id: i64) -> Result<PreviewInfo> {
        let row = self.store().task(id)?.ok_or_else(|| anyhow!("no such task: {id}"))?;
        let set = self.settings();
        let mut headers = row.headers.clone();
        let userinfo = headers.iter().position(|(k, _)| k.eq_ignore_ascii_case(AUTH_HEADER)).map(|i| headers.remove(i).1);
        headers.retain(|(k, _)| !k.eq_ignore_ascii_case(YTDLP_HEADER));
        let auth = userinfo.and_then(|v| v.split_once(':').map(|(u, p)| (u.replace("%3A", ":"), p.to_string()))).or_else(|| self.login_for(&row.info.url));
        let is_page = row.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(YTDLP_HEADER));
        let (mut name, mut size) = (row.info.filename.clone().unwrap_or_default(), None);
        if !is_page && !zdm_core::ftp::is_ftp_url(&row.info.url) {
            let mut opts = Options {
                auth: auth.clone(),
                creds: Arc::new(auth.as_ref().map_or_else(zdm_core::Credentials::default, |(u, p)| zdm_core::Credentials::new(u, p))),
                proxy: set.proxy.clone(),
                referer: row.referer.clone(),
                headers,
                ..Options::default()
            };
            if let Some(ua) = &row.user_agent {
                opts.user_agent = ua.clone();
            }
            let client = build_client(&opts)?;
            let p = tokio::time::timeout(Duration::from_secs(20), zdm_core::probe_with(&client, &row.info.url, &opts.creds)).await.map_err(|_| anyhow!("the server took too long to answer"))??;
            if name.is_empty() {
                name = p.filename.clone();
            }
            size = p.size;
        }
        if name.is_empty() {
            name = zdm_core::ftp::file_name(&row.info.url);
        }
        let category = categorize_with(&name, &set.category_exts);
        let dir = row.dest_dir.clone().unwrap_or_else(|| set.category_dirs.get(&category).cloned().unwrap_or_else(|| PathBuf::from(&set.download_root).join(&category).to_string_lossy().into_owned()));
        Ok(PreviewInfo { filename: name, size, category, dir })
    }

    pub fn set_target(&self, id: i64, filename: Option<&str>, dest_dir: Option<&str>) -> Result<()> {
        let st = self.get(id)?.state;
        if matches!(st, State::Running | State::Completed) {
            bail!("pause task {id} before changing where it is saved");
        }
        if let Some(f) = filename {
            if f.is_empty() || f.contains(['/', '\\']) || f == "." || f == ".." {
                bail!("filename must be a plain file name");
            }
        }
        if let Some(d) = dest_dir {
            if !Path::new(d).is_absolute() {
                bail!("the folder must be an absolute path");
            }
        }
        self.store().set_target(id, filename, dest_dir)
    }

    pub fn get(&self, id: i64) -> Result<TaskInfo> {
        self.store().task(id)?.map(|t| t.info).ok_or_else(|| anyhow!("no such task: {id}"))
    }

    pub fn list(&self) -> Result<Vec<TaskInfo>> {
        Ok(self.store().tasks()?.into_iter().map(|t| t.info).collect())
    }

    pub fn pause(&self, id: i64) -> Result<()> {
        let st = self.get(id)?.state;
        match st {
            State::Completed => bail!("task {id} is already completed"),
            State::Paused => {
                // "Download later" on a task that was waiting for the user's choice: keep it, drop the question
                if self.get(id)?.error.as_deref() == Some(ASK_MARKER) {
                    self.store().set_state(id, State::Paused, None)?;
                }
                return Ok(());
            }
            _ => {}
        }
        self.store().set_state(id, State::Paused, None)?;
        if let Some(r) = self.running.lock().unwrap().get(&id) {
            r.cancel.cancel();
        }
        Ok(())
    }

    pub fn resume(&self, id: i64) -> Result<()> {
        match self.get(id)?.state {
            State::Paused | State::Failed => {
                self.retries.lock().unwrap().remove(&id);
                self.store().set_state(id, State::Queued, None)?;
                self.wake.notify_one();
                Ok(())
            }
            State::Queued | State::Running => Ok(()),
            State::Completed => bail!("task {id} is already completed"),
        }
    }

    pub async fn remove(&self, id: i64, delete_files: bool) -> Result<()> {
        let info = self.get(id)?;
        let running = self.running.lock().unwrap().remove(&id);
        if let Some(r) = running {
            r.cancel.cancel();
            // Let the engine stop writing before files are touched.
            let _ = tokio::time::timeout(Duration::from_secs(10), r.handle).await;
        }
        self.store().delete_task(id)?;
        if delete_files {
            if let Some(d) = info.dest {
                let d = PathBuf::from(d);
                for p in [d.clone(), sibling(&d, ".part"), sibling(&d, ".part.zdm"), sibling(&d, ".part.zdm.tmp")] {
                    let _ = std::fs::remove_file(p);
                }
                let _ = std::fs::remove_dir_all(sibling(&d, ".hls"));
                let _ = std::fs::remove_dir_all(sibling(&d, ".ytdlp"));
            }
        }
        self.wake.notify_one();
        Ok(())
    }

    pub fn queue_list(&self) -> Result<Vec<QueueInfo>> {
        self.store().queues()
    }

    pub fn queue_set(&self, q: &QueueInfo) -> Result<()> {
        self.store().set_queue(q)?;
        if let Some(l) = self.queue_limits.lock().unwrap().get(&q.name) {
            l.set_rate(q.limit_kib * 1024);
        }
        self.wake.notify_one();
        Ok(())
    }

    pub fn queue_remove(&self, name: &str) -> Result<()> {
        self.store().remove_queue(name)?;
        self.wake.notify_one();
        Ok(())
    }

    // ---- scheduling ----

    /// One scheduling pass: stop tasks of closed queues and start queued tasks up to each queue's limit.
    pub fn tick(self: &Arc<Self>) -> Result<()> {
        let (minute, weekday) = (self.clock)();
        let queues = self.store().queues()?;
        for q in queues {
            let active = q.window.is_none_or(|w| w.active_at(minute, weekday));
            let mut running = self.running.lock().unwrap();
            let in_queue: Vec<i64> = running.iter().filter(|(_, r)| r.queue == q.name).map(|(id, _)| *id).collect();
            if !active {
                for id in in_queue {
                    if let Some(r) = running.get(&id) {
                        if !r.cancel.is_cancelled() {
                            r.cancel.cancel();
                            // Back to the line; it resumes when the window opens.
                            let _ = self.store().set_state(id, State::Queued, None);
                        }
                    }
                }
                continue;
            }
            let free = (q.max_parallel as usize).saturating_sub(in_queue.len());
            if free == 0 {
                continue;
            }
            // Tasks still winding down after a pause are 'queued' again but remain in `running`; skip them.
            let candidates = self.store().queued_in(&q.name, free + in_queue.len())?;
            let startable: Vec<i64> = candidates.into_iter().filter(|id| !running.contains_key(id)).take(free).collect();
            for id in startable {
                self.store().set_state(id, State::Running, None)?;
                let gen = self.gen.fetch_add(1, Ordering::Relaxed);
                let cancel = CancellationToken::new();
                let this = self.clone();
                let c = cancel.clone();
                let handle = tokio::spawn(async move { this.run_task(id, c, gen).await });
                running.insert(id, Running { gen, queue: q.name.clone(), cancel, handle });
            }
        }
        self.check_after_all();
        Ok(())
    }

    /// Scheduler loop; returns after `shutdown` fires and running downloads have saved their state.
    pub async fn run(self: Arc<Self>, shutdown: CancellationToken) {
        loop {
            if let Err(e) = self.tick() {
                eprintln!("scheduler error: {e:#}");
            }
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(self.cfg.tick) => {}
            }
        }
        let all: Vec<Running> = self.running.lock().unwrap().drain().map(|(_, r)| r).collect();
        for r in &all {
            r.cancel.cancel();
        }
        for r in all {
            let _ = r.handle.await;
        }
    }

    async fn run_task(self: Arc<Self>, id: i64, cancel: CancellationToken, gen: u64) {
        let outcome = self.execute(id, &cancel).await;
        {
            let mut r = self.running.lock().unwrap();
            if r.get(&id).is_some_and(|x| x.gen == gen) {
                r.remove(&id);
            }
        }
        let still_running = self.get(id).map(|t| t.state == State::Running).unwrap_or(false);
        match outcome {
            Ok(()) => {
                self.retries.lock().unwrap().remove(&id);
                {
                    let s = self.store();
                    if still_running {
                        let _ = s.set_state(id, State::Completed, None);
                    }
                    if let Ok(Some(t)) = s.task(id) {
                        if let Some(size) = t.info.size {
                            let _ = s.set_downloaded(id, size);
                        }
                    }
                }
                if still_running {
                    self.after.lock().unwrap().armed_with_work = true;
                    self.run_post_command(id);
                }
            }
            Err(e) => {
                let cancelled = cancel.is_cancelled() || e.downcast_ref::<Cancelled>().is_some();
                // Pause, queue-window stop and removal already recorded their own state.
                if !cancelled && still_running {
                    let msg = format!("{e:#}");
                    let max = self.settings().auto_retries;
                    let n = {
                        let mut r = self.retries.lock().unwrap();
                        let n = r.entry(id).or_insert(0);
                        *n += 1;
                        *n
                    };
                    if is_transient(&msg) && n <= max {
                        // a network problem: try again by itself after a growing pause
                        let delay = self.cfg.retry_delay * n;
                        self.retry_total.fetch_add(1, Ordering::Relaxed);
                        let _ = self.store().set_state(id, State::Failed, Some(&format!("{RETRY_PREFIX} {n} of {max}) in {}s: {msg}", delay.as_secs().max(1))));
                        let this = self.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(delay).await;
                            let still_waiting = this.get(id).is_ok_and(|t| t.state == State::Failed && t.error.as_deref().is_some_and(|e| e.starts_with(RETRY_PREFIX)));
                            if still_waiting {
                                let _ = this.store().set_state(id, State::Queued, None);
                                this.wake.notify_one();
                            }
                        });
                    } else {
                        self.retries.lock().unwrap().remove(&id);
                        let _ = self.store().set_state(id, State::Failed, Some(&msg));
                    }
                }
            }
        }
        self.wake.notify_one();
        self.check_after_all();
    }

    /// Run the user's after-download command (virus scan, move, notify...) for a finished download.
    /// The file name is passed in `$ZDM_FILE`, never spliced into the command text. A non-zero exit is noted
    /// on the task (it stays completed, the file is kept).
    fn run_post_command(self: &Arc<Self>, id: i64) {
        let Some(cmd) = self.settings().post_command else { return };
        let Ok(t) = self.get(id) else { return };
        let Some(dest) = t.dest.clone() else { return };
        let this = self.clone();
        std::thread::spawn(move || {
            use std::process::{Command, Stdio};
            let note = |msg: String| {
                let _ = this.store().set_state(id, State::Completed, Some(&msg));
            };
            let child = Command::new("sh").arg("-c").arg(&cmd).env("ZDM_FILE", &dest).env("ZDM_URL", &t.url).env("ZDM_TASK", id.to_string()).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
            let mut child = match child {
                Ok(c) => c,
                Err(e) => return note(format!("after-download command could not start: {e}")),
            };
            let started = std::time::Instant::now();
            loop {
                match child.try_wait() {
                    Ok(Some(st)) if st.success() => return,
                    Ok(Some(st)) => return note(format!("after-download command exited with {}", st.code().map_or("a signal".to_string(), |c| c.to_string()))),
                    Ok(None) if started.elapsed() > Duration::from_secs(600) => {
                        let _ = child.kill();
                        return note("after-download command took more than 10 minutes and was stopped".into());
                    }
                    Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                    Err(_) => return,
                }
            }
        });
    }

    async fn execute(self: &Arc<Self>, id: i64, cancel: &CancellationToken) -> Result<()> {
        let row = self.store().task(id)?.ok_or_else(|| anyhow!("task {id} vanished"))?;
        let set = self.settings();
        // stored credentials: Basic is sent at once, Digest is answered when the server asks for it
        let mut headers = row.headers.clone();
        let userinfo = headers.iter().position(|(k, _)| k.eq_ignore_ascii_case(AUTH_HEADER)).map(|i| headers.remove(i).1);
        headers.retain(|(k, _)| !k.eq_ignore_ascii_case(YTDLP_HEADER)); // ours, not a request header
        let auth = userinfo.and_then(|v| v.split_once(':').map(|(u, p)| (u.replace("%3A", ":"), p.to_string())));
        let auth = auth.or_else(|| self.login_for(&row.info.url)); // a saved site login, when the task has none of its own
        let mut opts = Options {
            auth: auth.clone(),
            creds: Arc::new(auth.as_ref().map_or_else(zdm_core::Credentials::default, |(u, p)| zdm_core::Credentials::new(u, p))),
            connections: set.connections as usize,
            limit_bps: if set.speed_limit_kib > 0 { Some(set.speed_limit_kib * 1024) } else { self.cfg.limit_bps },
            proxy: set.proxy.clone(),
            shared_limits: self.limiters_for(&row.info.queue),
            referer: row.referer.clone(),
            headers,
            cancel: cancel.clone(),
            ..Options::default()
        };
        if let Some(ua) = &row.user_agent {
            opts.user_agent = ua.clone();
        }
        if let Some(sel) = row.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(YTDLP_HEADER)).map(|(_, v)| v.clone()) {
            return self.execute_ytdlp(id, &row, &sel, &opts, &set, cancel).await;
        }
        let client = build_client(&opts)?;
        if zdm_core::ftp::is_ftp_url(&row.info.url) {
            return self.execute_ftp(id, &row, &opts).await;
        }
        if zdm_core::dash::is_dash_url(&row.info.url) {
            return self.execute_dash(id, &row, &client, cancel).await;
        }
        if zdm_core::hls::is_hls_url(&row.info.url) {
            return self.execute_hls(id, &row, &client, cancel, set.connections as usize).await;
        }
        let p = tokio::select! {
            _ = cancel.cancelled() => return Err(Cancelled.into()),
            p = zdm_core::probe_with(&client, &row.info.url, &opts.creds) => p?,
        };
        let dest = match &row.info.dest {
            Some(d) => {
                self.store().set_size(id, p.size)?;
                PathBuf::from(d)
            }
            None => self.choose_dest(id, &row.info.filename.clone().unwrap_or_else(|| p.filename.clone()), row.dest_dir.as_deref(), p.size)?,
        };
        let this = self.clone();
        download_probed(&client, &row.info.url, &p, &dest, &opts, move |done, _| {
            let _ = this.store().set_downloaded(id, done);
        })
        .await
    }

    /// A video page: yt-dlp fetches the chosen format (and joins video + audio with ffmpeg).
    async fn execute_ytdlp(self: &Arc<Self>, id: i64, row: &crate::store::TaskRow, sel: &str, opts: &Options, set: &Settings, cancel: &CancellationToken) -> Result<()> {
        let dest = match &row.info.dest {
            Some(d) => PathBuf::from(d),
            None => {
                let name = row.info.filename.clone().unwrap_or_else(|| "video.mp4".into());
                self.choose_dest(id, &name, row.dest_dir.as_deref(), None)?
            }
        };
        let queue_limit = self.store().queue(&row.info.queue).ok().flatten().map_or(0, |q| q.limit_kib);
        // yt-dlp is a separate program, so the shared limiters cannot pace it: give it the strictest cap
        let limit_kib = [set.speed_limit_kib, set.global_limit_kib, queue_limit].into_iter().filter(|k| *k > 0).min().unwrap_or(0);
        let headers: Vec<(String, String)> = opts.headers.clone();
        let this = self.clone();
        let done = crate::ytdlp::download(
            crate::ytdlp::YtdlpJob { page_url: &row.info.url, selector: sel, dest: &dest, proxy: set.proxy.as_deref(), referer: opts.referer.as_deref(), user_agent: row.user_agent.as_deref(), headers: &headers, limit_kib, cookie_browser: set.ytdlp_cookies_browser.as_deref(), cancel },
            move |d, t| {
                let s = this.store();
                let _ = s.set_downloaded(id, d);
                if t > 0 {
                    let _ = s.set_size(id, Some(t));
                }
            },
        )
        .await?;
        let len = std::fs::metadata(&done)?.len();
        let name = done.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let s = self.store();
        s.set_meta(id, &name, &done.to_string_lossy(), &categorize_with(&name, &self.settings().category_exts), Some(len))?;
        s.set_downloaded(id, len)?;
        Ok(())
    }

    /// An `ftp://` file: one resumable stream.
    async fn execute_ftp(self: &Arc<Self>, id: i64, row: &crate::store::TaskRow, opts: &Options) -> Result<()> {
        let dest = match &row.info.dest {
            Some(d) => PathBuf::from(d),
            None => {
                let name = row.info.filename.clone().unwrap_or_else(|| zdm_core::ftp::file_name(&row.info.url));
                self.choose_dest(id, &name, row.dest_dir.as_deref(), None)?
            }
        };
        let this = self.clone();
        zdm_core::ftp::download_ftp(&row.info.url, &dest, opts, move |done, total| {
            let s = this.store();
            let _ = s.set_downloaded(id, done);
            if total > 0 {
                let _ = s.set_size(id, Some(total));
            }
        })
        .await?;
        let len = std::fs::metadata(&dest)?.len();
        let s = self.store();
        s.set_size(id, Some(len))?;
        s.set_downloaded(id, len)?;
        Ok(())
    }

    /// A DASH video (`.mpd`), fetched and converted to MP4 by ffmpeg. No partial resume.
    async fn execute_dash(self: &Arc<Self>, id: i64, row: &crate::store::TaskRow, client: &reqwest::Client, cancel: &CancellationToken) -> Result<()> {
        let set = self.settings();
        let dest = match &row.info.dest {
            Some(d) => PathBuf::from(d),
            None => {
                let name = row.info.filename.clone().unwrap_or_else(|| format!("{}.mp4", hls_stem(&row.info.url)));
                self.choose_dest(id, &name, row.dest_dir.as_deref(), None)?
            }
        };
        let mut headers = row.headers.clone();
        let userinfo = headers.iter().position(|(k, _)| k.eq_ignore_ascii_case(AUTH_HEADER)).map(|i| headers.remove(i).1);
        let opts = Options {
            auth: userinfo.and_then(|v| v.split_once(':').map(|(u, p)| (u.replace("%3A", ":"), p.to_string()))).or_else(|| self.login_for(&row.info.url)),
            proxy: set.proxy.clone(),
            referer: row.referer.clone(),
            headers,
            user_agent: row.user_agent.clone().unwrap_or_else(|| Options::default().user_agent),
            cancel: cancel.clone(),
            ..Options::default()
        };
        let this = self.clone();
        zdm_core::dash::download_dash(client, &row.info.url, &dest, &opts, move |done, total| {
            let s = this.store();
            let _ = s.set_downloaded(id, done);
            if total > 0 {
                let _ = s.set_size(id, Some(total));
            }
        })
        .await?;
        let len = std::fs::metadata(&dest)?.len();
        let s = self.store();
        s.set_size(id, Some(len))?;
        s.set_downloaded(id, len)?;
        Ok(())
    }

    /// A video stream (`.m3u8`): segments are fetched in parallel, joined, and remuxed to MP4 when ffmpeg exists.
    async fn execute_hls(self: &Arc<Self>, id: i64, row: &crate::store::TaskRow, client: &reqwest::Client, cancel: &CancellationToken, connections: usize) -> Result<()> {
        let dest = match &row.info.dest {
            Some(d) => PathBuf::from(d),
            None => {
                let ext = if zdm_core::hls::ffmpeg_path().is_some() { "mp4" } else { "ts" };
                let name = row.info.filename.clone().unwrap_or_else(|| format!("{}.{ext}", hls_stem(&row.info.url)));
                self.choose_dest(id, &name, row.dest_dir.as_deref(), None)?
            }
        };
        let this = self.clone();
        let opts = zdm_core::hls::HlsOptions { cancel: cancel.clone(), connections: connections.min(8), remux: true, variant: None };
        let done = zdm_core::hls::download_hls(client, &row.info.url, &dest, &opts, move |done, total| {
            let s = this.store();
            let _ = s.set_downloaded(id, done);
            if total > 0 {
                let _ = s.set_size(id, Some(total));
            }
        })
        .await?;
        // the result can differ from the plan (.mp4 -> .ts when ffmpeg could not convert): record the truth
        let len = std::fs::metadata(&done)?.len();
        let name = done.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let s = self.store();
        s.set_meta(id, &name, &done.to_string_lossy(), &categorize_with(&name, &self.settings().category_exts), Some(len))?;
        s.set_downloaded(id, len)?;
        Ok(())
    }

    /// Pick a unique destination path under the category folder and record it before downloading.
    fn choose_dest(&self, id: i64, filename: &str, dest_dir: Option<&str>, size: Option<u64>) -> Result<PathBuf> {
        let set = self.settings();
        let category = categorize_with(filename, &set.category_exts);
        let dir = dest_dir.map(PathBuf::from).unwrap_or_else(|| set.category_dirs.get(&category).map(PathBuf::from).unwrap_or_else(|| PathBuf::from(&set.download_root).join(&category)));
        std::fs::create_dir_all(&dir)?;
        let store = self.store();
        let claimed = store.claimed_dests()?;
        let wanted = dir.join(filename);
        let taken_by_task = |p: &Path| claimed.iter().any(|c| Path::new(c) == p);
        let path = match set.on_exists.as_str() {
            // replace an existing file, unless another download in the list is already going to that name
            "overwrite" if !taken_by_task(&wanted) || !store.tasks()?.iter().any(|t| t.info.state != State::Completed && t.info.dest.as_deref().is_some_and(|d| Path::new(d) == wanted)) => wanted,
            "skip" if wanted.exists() => bail!("{} already exists (skipped by your settings)", wanted.display()),
            _ => unique_path(&dir, filename, |p| taken_by_task(p) || p.exists() || sibling(p, ".part").exists()),
        };
        store.set_meta(id, filename, &path.to_string_lossy(), &category, size)?;
        Ok(path)
    }

}

fn sibling(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// `name.ext`, then `name (1).ext`, `name (2).ext`, ... until `taken` says no.
fn unique_path(dir: &Path, filename: &str, taken: impl Fn(&Path) -> bool) -> PathBuf {
    let first = dir.join(filename);
    if !taken(&first) {
        return first;
    }
    let (stem, ext) = match filename.rfind('.') {
        Some(i) if i > 0 => (&filename[..i], &filename[i..]),
        _ => (filename, ""),
    };
    (1..).map(|n| dir.join(format!("{stem} ({n}){ext}"))).find(|p| !taken(p)).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_names() {
        let dir = Path::new("d");
        let taken = |p: &Path| p.ends_with("a.zip") || p.ends_with("a (1).zip");
        assert_eq!(unique_path(dir, "a.zip", taken), dir.join("a (2).zip"));
        assert_eq!(unique_path(dir, "b", |_| false), dir.join("b"));
        assert_eq!(unique_path(dir, "noext", |p| p.ends_with("noext")), dir.join("noext (1)"));
    }
}
