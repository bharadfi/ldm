use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use zdm_core::{build_client, download_probed, probe, Options};

/// Segmented, resumable downloader.
#[derive(Parser)]
#[command(name = "zdm", version)]
struct Args {
    url: String,
    /// Output file (default: name from the server, in the current directory)
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// Maximum parallel connections
    #[arg(short = 'n', long, default_value_t = 8)]
    connections: usize,
    #[arg(long)]
    referer: Option<String>,
    #[arg(long)]
    user_agent: Option<String>,
    /// Extra header, "Name: value" (repeatable)
    #[arg(short = 'H', long = "header")]
    headers: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let a = Args::parse();
    let mut opts = Options { connections: a.connections, referer: a.referer, ..Options::default() };
    if let Some(ua) = a.user_agent {
        opts.user_agent = ua;
    }
    for h in &a.headers {
        let (k, v) = h.split_once(':').ok_or_else(|| anyhow::anyhow!("header must be 'Name: value': {h}"))?;
        opts.headers.push((k.trim().to_string(), v.trim().to_string()));
    }
    let client = build_client(&opts)?;
    let p = probe(&client, &a.url).await?;
    let dest = a.output.unwrap_or_else(|| PathBuf::from(&p.filename));
    eprintln!(
        "{} -> {} ({} bytes, ranges: {})",
        a.url,
        dest.display(),
        p.size.map_or("unknown".into(), |s| s.to_string()),
        p.ranges
    );
    download_probed(&client, &a.url, &p, &dest, &opts, |done, total| {
        if total > 0 {
            eprint!("\r{:>6.1}%  {} / {} bytes", done as f64 * 100.0 / total as f64, done, total);
        } else {
            eprint!("\r{done} bytes");
        }
    })
    .await?;
    eprintln!("\ndone");
    Ok(())
}
