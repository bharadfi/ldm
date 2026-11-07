//! SQLite persistence for tasks and queues.

use std::path::Path;

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OptionalExtension, Row};
use zdm_proto::{QueueInfo, State, TaskInfo, Window};

pub const DEFAULT_QUEUE: &str = "Main";

/// A task plus the request details the API does not expose.
#[derive(Debug, Clone)]
pub struct TaskRow {
    pub info: TaskInfo,
    pub dest_dir: Option<String>,
    pub referer: Option<String>,
    pub headers: Vec<(String, String)>,
}

pub struct NewTask<'a> {
    pub url: &'a str,
    pub filename: Option<&'a str>,
    pub dest_dir: Option<&'a str>,
    pub queue: &'a str,
    pub state: State,
    pub referer: Option<&'a str>,
    pub headers: &'a [(String, String)],
}

pub struct Store {
    db: Connection,
}

const TASK_COLS: &str = "id,url,filename,dest,category,queue,state,size,downloaded,error,added_at,dest_dir,referer,headers";

fn row_to_task(r: &Row) -> rusqlite::Result<TaskRow> {
    let state: String = r.get(6)?;
    let headers: String = r.get(13)?;
    Ok(TaskRow {
        info: TaskInfo {
            id: r.get(0)?,
            url: r.get(1)?,
            filename: r.get(2)?,
            dest: r.get(3)?,
            category: r.get(4)?,
            queue: r.get(5)?,
            state: State::parse(&state).unwrap_or(State::Failed),
            size: r.get::<_, Option<i64>>(7)?.map(|v| v as u64),
            downloaded: r.get::<_, i64>(8)? as u64,
            error: r.get(9)?,
            added_at: r.get(10)?,
        },
        dest_dir: r.get(11)?,
        referer: r.get(12)?,
        headers: serde_json::from_str(&headers).unwrap_or_default(),
    })
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Tasks may carry cookies; keep the database private to the user.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(path)?;
        }
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(db: Connection) -> Result<Self> {
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS queues(
                 name TEXT PRIMARY KEY, max_parallel INTEGER NOT NULL,
                 win_start INTEGER, win_end INTEGER, days INTEGER NOT NULL DEFAULT 127);
             CREATE TABLE IF NOT EXISTS tasks(
                 id INTEGER PRIMARY KEY AUTOINCREMENT, url TEXT NOT NULL, filename TEXT, dest TEXT,
                 category TEXT, queue TEXT NOT NULL, state TEXT NOT NULL, size INTEGER,
                 downloaded INTEGER NOT NULL DEFAULT 0, error TEXT, added_at INTEGER NOT NULL,
                 dest_dir TEXT, referer TEXT, headers TEXT NOT NULL DEFAULT '[]');",
        )?;
        db.execute("INSERT OR IGNORE INTO queues(name,max_parallel,days) VALUES(?,3,127)", [DEFAULT_QUEUE])?;
        let s = Self { db };
        // A previous daemon run may have died mid-download; those tasks resume from their sidecars.
        s.db.execute("UPDATE tasks SET state='queued' WHERE state='running'", [])?;
        Ok(s)
    }

    pub fn insert_task(&self, t: &NewTask) -> Result<i64> {
        self.db.execute(
            "INSERT INTO tasks(url,filename,dest_dir,queue,state,referer,headers,added_at) VALUES(?,?,?,?,?,?,?,strftime('%s','now'))",
            params![t.url, t.filename, t.dest_dir, t.queue, t.state.as_str(), t.referer, serde_json::to_string(t.headers)?],
        )?;
        Ok(self.db.last_insert_rowid())
    }

    pub fn task(&self, id: i64) -> Result<Option<TaskRow>> {
        Ok(self.db.query_row(&format!("SELECT {TASK_COLS} FROM tasks WHERE id=?"), [id], row_to_task).optional()?)
    }

    pub fn tasks(&self) -> Result<Vec<TaskRow>> {
        let mut st = self.db.prepare(&format!("SELECT {TASK_COLS} FROM tasks ORDER BY id"))?;
        let rows = st.query_map([], row_to_task)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn queued_in(&self, queue: &str, limit: usize) -> Result<Vec<i64>> {
        let mut st = self.db.prepare("SELECT id FROM tasks WHERE queue=? AND state='queued' ORDER BY id LIMIT ?")?;
        let ids = st.query_map(params![queue, limit as i64], |r| r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ids)
    }

    pub fn set_state(&self, id: i64, state: State, error: Option<&str>) -> Result<()> {
        self.db.execute("UPDATE tasks SET state=?, error=? WHERE id=?", params![state.as_str(), error, id])?;
        Ok(())
    }

    pub fn set_meta(&self, id: i64, filename: &str, dest: &str, category: &str, size: Option<u64>) -> Result<()> {
        self.db.execute(
            "UPDATE tasks SET filename=?, dest=?, category=?, size=? WHERE id=?",
            params![filename, dest, category, size.map(|s| s as i64), id],
        )?;
        Ok(())
    }

    pub fn set_size(&self, id: i64, size: Option<u64>) -> Result<()> {
        self.db.execute("UPDATE tasks SET size=? WHERE id=?", params![size.map(|s| s as i64), id])?;
        Ok(())
    }

    pub fn set_downloaded(&self, id: i64, downloaded: u64) -> Result<()> {
        self.db.execute("UPDATE tasks SET downloaded=? WHERE id=?", params![downloaded as i64, id])?;
        Ok(())
    }

    pub fn delete_task(&self, id: i64) -> Result<()> {
        self.db.execute("DELETE FROM tasks WHERE id=?", [id])?;
        Ok(())
    }

    /// Destination paths already claimed by tasks (so concurrent adds of the same name do not collide).
    pub fn claimed_dests(&self) -> Result<Vec<String>> {
        let mut st = self.db.prepare("SELECT dest FROM tasks WHERE dest IS NOT NULL")?;
        let v = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
        Ok(v)
    }

    pub fn queues(&self) -> Result<Vec<QueueInfo>> {
        let mut st = self.db.prepare("SELECT name,max_parallel,win_start,win_end,days FROM queues ORDER BY name")?;
        let v = st
            .query_map([], |r| {
                let (s, e): (Option<u32>, Option<u32>) = (r.get(2)?, r.get(3)?);
                Ok(QueueInfo {
                    name: r.get(0)?,
                    max_parallel: r.get(1)?,
                    window: s.zip(e).map(|(start_min, end_min)| Window { start_min, end_min, days: r.get::<_, u8>(4).unwrap_or(127) }),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    }

    pub fn queue(&self, name: &str) -> Result<Option<QueueInfo>> {
        Ok(self.queues()?.into_iter().find(|q| q.name == name))
    }

    pub fn set_queue(&self, q: &QueueInfo) -> Result<()> {
        if q.name.trim().is_empty() {
            return Err(anyhow!("queue name must not be empty"));
        }
        if q.max_parallel == 0 {
            return Err(anyhow!("max_parallel must be at least 1"));
        }
        if let Some(w) = &q.window {
            if w.start_min >= 1440 || w.end_min >= 1440 {
                return Err(anyhow!("window minutes must be below 1440"));
            }
        }
        self.db.execute(
            "INSERT INTO queues(name,max_parallel,win_start,win_end,days) VALUES(?,?,?,?,?)
             ON CONFLICT(name) DO UPDATE SET max_parallel=excluded.max_parallel, win_start=excluded.win_start,
             win_end=excluded.win_end, days=excluded.days",
            params![q.name, q.max_parallel, q.window.map(|w| w.start_min), q.window.map(|w| w.end_min), q.window.map_or(127, |w| w.days)],
        )?;
        Ok(())
    }

    /// Delete a queue; its tasks move to the default queue.
    pub fn remove_queue(&self, name: &str) -> Result<()> {
        if name == DEFAULT_QUEUE {
            return Err(anyhow!("the default queue cannot be removed"));
        }
        self.db.execute("UPDATE tasks SET queue=? WHERE queue=?", params![DEFAULT_QUEUE, name])?;
        self.db.execute("DELETE FROM queues WHERE name=?", [name])?;
        Ok(())
    }
}
