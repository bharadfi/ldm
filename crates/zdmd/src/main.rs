use anyhow::Result;
use tokio_util::sync::CancellationToken;
use zdmd::{config::Config, manager::Manager, server};

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = Config::from_env();
    eprintln!("zdmd {} — data: {}, downloads: {}", env!("CARGO_PKG_VERSION"), cfg.db_path.display(), cfg.download_root.display());
    let manager = Manager::open(cfg)?;
    let shutdown = CancellationToken::new();

    let sched = tokio::spawn(manager.clone().run(shutdown.clone()));
    let srv = tokio::spawn(server::listen(manager.clone(), shutdown.clone()));

    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            r = tokio::signal::ctrl_c() => r?,
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    eprintln!("shutting down, saving download state...");
    shutdown.cancel();
    let _ = sched.await;
    let _ = srv.await;
    Ok(())
}
