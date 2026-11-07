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
    /// Extra header, "Name: value" (repeatable), e.g. -H "Cookie: a=b"
    #[arg(short = 'H', long = "header")]
    headers: Vec<String>,
    /// Proxy URL, e.g. http://host:3128 or socks5://host:1080
    #[arg(long)]
    proxy: Option<String>,
    /// HTTP Basic credentials as USER:PASSWORD (visible in the process list; prefer ZDM_AUTH)
    #[arg(long, env = "ZDM_AUTH", hide_env_values = true)]
    user: Option<String>,
    /// Rate limit across all connections, e.g. 500k or 2m (bytes per second)
    #[arg(long)]
    limit_rate: Option<String>,
}

fn parse_rate(s: &str) -> Result<u64> {
    let s = s.trim().to_ascii_lowercase();
    let (num, mult) = match s.chars().last() {
        Some('k') => (&s[..s.len() - 1], 1u64 << 10),
        Some('m') => (&s[..s.len() - 1], 1 << 20),
        Some('g') => (&s[..s.len() - 1], 1 << 30),
        _ => (s.as_str(), 1),
    };
    let n: f64 = num.parse().map_err(|_| anyhow::anyhow!("bad rate: {s}"))?;
    anyhow::ensure!(n > 0.0, "rate must be positive");
    Ok((n * mult as f64) as u64)
}

#[tokio::main]
async fn main() -> Result<()> {
    let a = Args::parse();
    let mut opts = Options { connections: a.connections, referer: a.referer, ..Options::default() };
    if let Some(ua) = a.user_agent {
        opts.user_agent = ua;
    }
    opts.proxy = a.proxy;
    if let Some(u) = a.user {
        let (user, pass) = u.split_once(':').unwrap_or((u.as_str(), ""));
        opts.auth = Some((user.to_string(), pass.to_string()));
    }
    if let Some(r) = &a.limit_rate {
        opts.limit_bps = Some(parse_rate(r)?);
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
