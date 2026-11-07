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
use zdm_core::{build_client, download_probed, probe, Cancelled, Options};
use zdm_proto::{AddRequest, QueueInfo, State, TaskInfo};

use crate::categories::categorize;
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

pub struct Manager {
    cfg: Config,
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
        Arc::new(Self { cfg, store: Mutex::new(store), running: Mutex::new(HashMap::new()), gen: AtomicU64::new(1), wake: Notify::new(), clock })
    }

    fn store(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store.lock().unwrap()
    }

    // ---- API ----

    pub fn add(&self, a: AddRequest) -> Result<TaskInfo> {
        if !(a.url.starts_with("http://") || a.url.starts_with("https://")) {
            bail!("only http:// and https:// URLs are supported");
        }
        if let Some(f) = &a.filename {
            if f.is_empty() || f.contains(['/', '\\']) || f == "." || f == ".." {
                bail!("filename must be a plain file name");
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
                headers: &a.headers,
            })?
        };
        self.wake.notify_one();
        self.get(id)
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
            State::Paused => return Ok(()),
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
            Err(e) => {
                let cancelled = cancel.is_cancelled() || e.downcast_ref::<Cancelled>().is_some();
                // Pause, queue-window stop and removal already recorded their own state.
                if !cancelled && still_running {
                    let _ = self.store().set_state(id, State::Failed, Some(&format!("{e:#}")));
                }
            }
        }
        self.wake.notify_one();
    }

    async fn execute(self: &Arc<Self>, id: i64, cancel: &CancellationToken) -> Result<()> {
        let row = self.store().task(id)?.ok_or_else(|| anyhow!("task {id} vanished"))?;
        let opts = Options {
            connections: self.cfg.connections,
            limit_bps: self.cfg.limit_bps,
            referer: row.referer.clone(),
            headers: row.headers.clone(),
            cancel: cancel.clone(),
            ..Options::default()
        };
        let client = build_client(&opts)?;
        let p = tokio::select! {
            _ = cancel.cancelled() => return Err(Cancelled.into()),
            p = probe(&client, &row.info.url) => p?,
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

    /// Pick a unique destination path under the category folder and record it before downloading.
    fn choose_dest(&self, id: i64, filename: &str, dest_dir: Option<&str>, size: Option<u64>) -> Result<PathBuf> {
        let category = categorize(filename);
        let dir = dest_dir.map(PathBuf::from).unwrap_or_else(|| self.cfg.download_root.join(category));
        std::fs::create_dir_all(&dir)?;
        let store = self.store();
        let claimed = store.claimed_dests()?;
        let path = unique_path(&dir, filename, |p| claimed.iter().any(|c| Path::new(c) == p) || p.exists() || sibling(p, ".part").exists());
        store.set_meta(id, filename, &path.to_string_lossy(), category, size)?;
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
