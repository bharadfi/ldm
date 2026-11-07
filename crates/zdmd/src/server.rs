//! Line-delimited JSON control server (see `zdm-proto`).

use std::sync::Arc;

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;
use zdm_proto::{Request, Response};

use crate::manager::Manager;

/// Execute one request.
pub async fn dispatch(m: &Arc<Manager>, req: Request) -> Response {
    let r: Result<Response> = async {
        Ok(match req {
            Request::Ping => Response::Pong { version: env!("CARGO_PKG_VERSION").to_string() },
            Request::Add(a) => Response::Task(m.add(a)?),
            Request::List => Response::Tasks { tasks: m.list()? },
            Request::Get { id } => Response::Task(m.get(id)?),
            Request::Pause { id } => {
                m.pause(id)?;
                Response::Ok
            }
            Request::Resume { id } => {
                m.resume(id)?;
                Response::Ok
            }
            Request::Remove { id, delete_files } => {
                m.remove(id, delete_files).await?;
                Response::Ok
            }
            Request::QueueList => Response::Queues { queues: m.queue_list()? },
            Request::QueueSet(q) => {
                m.queue_set(&q)?;
                Response::Ok
            }
            Request::QueueRemove { name } => {
                m.queue_remove(&name)?;
                Response::Ok
            }
        })
    }
    .await;
    r.unwrap_or_else(|e| Response::Error { message: format!("{e:#}") })
}

/// Serve one client connection until it closes. Lines over 1 MiB are rejected.
pub async fn serve_conn<S: AsyncRead + AsyncWrite + Unpin>(m: Arc<Manager>, stream: S) -> Result<()> {
    let mut r = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        let n = (&mut r).take(1 << 20).read_line(&mut line).await?;
        if n == 0 {
            return Ok(());
        }
        let resp = match serde_json::from_str::<Request>(&line) {
            Ok(req) => dispatch(&m, req).await,
            Err(e) => Response::Error { message: format!("bad request: {e}") },
        };
        let mut out = serde_json::to_vec(&resp)?;
        out.push(b'\n');
        r.get_mut().write_all(&out).await?;
    }
}

/// Listen on the per-user endpoint until `shutdown` fires.
#[cfg(unix)]
pub async fn listen(m: Arc<Manager>, shutdown: CancellationToken) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let path = zdm_proto::socket_path();
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    // A stale socket from a crashed daemon blocks bind; a live one means another daemon is running.
    if path.exists() {
        if tokio::net::UnixStream::connect(&path).await.is_ok() {
            anyhow::bail!("zdmd is already running ({})", path.display());
        }
        std::fs::remove_file(&path)?;
    }
    let listener = tokio::net::UnixListener::bind(&path)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            acc = listener.accept() => {
                let (s, _) = acc?;
                let m = m.clone();
                tokio::spawn(async move { let _ = serve_conn(m, s).await; });
            }
        }
    }
    let _ = std::fs::remove_file(&path);
    Ok(())
}

/// Windows development builds: loopback TCP with a random token as the first line of every connection.
#[cfg(not(unix))]
pub async fn listen(m: Arc<Manager>, shutdown: CancellationToken) -> Result<()> {
    // Refuse to start a second daemon on the same database (it would also replace daemon.json).
    if zdm_proto::daemon_info_path().exists() && zdm_proto::connect().await.is_ok() {
        anyhow::bail!("zdmd is already running");
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let token: String = rand::random::<[u8; 16]>().iter().map(|b| format!("{b:02x}")).collect();
    let info = zdm_proto::DaemonInfo { addr: listener.local_addr()?.to_string(), token: token.clone() };
    let path = zdm_proto::daemon_info_path();
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, serde_json::to_vec(&info)?)?;
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            acc = listener.accept() => {
                let (s, _) = acc?;
                let (m, token) = (m.clone(), token.clone());
                tokio::spawn(async move {
                    let mut r = BufReader::new(s);
                    let mut first = String::new();
                    // Reject anything (e.g. a web page poking at localhost) that does not start with the token.
                    if (&mut r).take(256).read_line(&mut first).await.is_err() || first.trim() != token {
                        return;
                    }
                    let _ = serve_conn(m, r).await;
                });
            }
        }
    }
    let _ = std::fs::remove_file(&path);
    Ok(())
}
