use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use zdm_core::{build_client, download_probed, probe, Options};
use zdm_proto::{call, AddRequest, QueueInfo, Request, Response, TaskInfo, Window};

/// Segmented, resumable downloader.
#[derive(Parser)]
#[command(name = "zdm", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Download a URL right now, without the daemon
    Get(GetArgs),
    /// Add a download to the daemon's queue
    Add {
        url: String,
        #[arg(short, long)]
        filename: Option<String>,
        /// Folder to save into (default: the file's category folder)
        #[arg(short = 'd', long)]
        dest_dir: Option<String>,
        #[arg(short, long)]
        queue: Option<String>,
        #[arg(long)]
        referer: Option<String>,
        /// Extra header "Name: value" (repeatable), e.g. -H "Cookie: a=b"
        #[arg(short = 'H', long = "header")]
        headers: Vec<String>,
        /// Add paused instead of starting
        #[arg(long)]
        paused: bool,
    },
    /// List downloads
    List,
    /// Show one download
    Show { id: i64 },
    Pause { id: i64 },
    Resume { id: i64 },
    /// Remove a download from the list
    Rm {
        id: i64,
        /// Also delete the downloaded and partial files
        #[arg(long)]
        delete_files: bool,
    },
    /// Manage queues
    Queue {
        #[command(subcommand)]
        cmd: QueueCmd,
    },
    /// Check that the daemon is running
    Ping,
}

#[derive(Subcommand)]
enum QueueCmd {
    List,
    /// Create or update a queue
    Set {
        name: String,
        /// Simultaneous downloads
        #[arg(short = 'n', long, default_value_t = 3)]
        parallel: u32,
        /// Allowed time window, HH:MM-HH:MM local time (may span midnight)
        #[arg(long)]
        window: Option<String>,
        /// Allowed days, comma separated: mon,tue,... (default: every day)
        #[arg(long)]
        days: Option<String>,
    },
    Rm { name: String },
}

#[derive(clap::Args)]
struct GetArgs {
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

fn parse_headers(hs: &[String]) -> Result<Vec<(String, String)>> {
    hs.iter()
        .map(|h| {
            let (k, v) = h.split_once(':').ok_or_else(|| anyhow::anyhow!("header must be 'Name: value': {h}"))?;
            Ok((k.trim().to_string(), v.trim().to_string()))
        })
        .collect()
}

fn parse_hhmm(s: &str) -> Result<u32> {
    let (h, m) = s.split_once(':').ok_or_else(|| anyhow::anyhow!("time must be HH:MM: {s}"))?;
    let (h, m): (u32, u32) = (h.parse()?, m.parse()?);
    anyhow::ensure!(h < 24 && m < 60, "time out of range: {s}");
    Ok(h * 60 + m)
}

fn parse_days(s: &str) -> Result<u8> {
    let names = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
    let mut bits = 0u8;
    for d in s.split(',') {
        let d = d.trim().to_ascii_lowercase();
        let i = names.iter().position(|n| d.starts_with(n)).ok_or_else(|| anyhow::anyhow!("unknown day: {d}"))?;
        bits |= 1 << i;
    }
    Ok(bits)
}

fn fmt_size(n: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let (mut v, mut i) = (n as f64, 0);
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{n} B") } else { format!("{v:.1} {}", U[i]) }
}

fn print_task(t: &TaskInfo) {
    let pct = t.size.filter(|s| *s > 0).map_or("   ?".to_string(), |s| format!("{:>3}%", t.downloaded * 100 / s));
    println!(
        "{:>4}  {:<9} {}  {:>9}  [{}] {}{}",
        t.id,
        t.state.as_str(),
        pct,
        t.size.map_or("?".into(), fmt_size),
        t.queue,
        t.filename.as_deref().unwrap_or(&t.url),
        t.error.as_ref().map_or(String::new(), |e| format!("  — {e}")),
    );
}

async fn get(a: GetArgs) -> Result<()> {
    let mut opts = Options { connections: a.connections, referer: a.referer, ..Options::default() };
    if let Some(ua) = a.user_agent {
        opts.user_agent = ua;
    }
    opts.headers = parse_headers(&a.headers)?;
    opts.proxy = a.proxy;
    if let Some(u) = a.user {
        let (user, pass) = u.split_once(':').unwrap_or((u.as_str(), ""));
        opts.auth = Some((user.to_string(), pass.to_string()));
    }
    if let Some(r) = &a.limit_rate {
        opts.limit_bps = Some(parse_rate(r)?);
    }
    let client = build_client(&opts)?;
    let p = probe(&client, &a.url).await?;
    let dest = a.output.unwrap_or_else(|| PathBuf::from(&p.filename));
    eprintln!("{} -> {} ({} bytes, ranges: {})", a.url, dest.display(), p.size.map_or("unknown".into(), |s| s.to_string()), p.ranges);
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

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Get(a) => get(a).await,
        Cmd::Add { url, filename, dest_dir, queue, referer, headers, paused } => {
            let r = call(&Request::Add(AddRequest { url, filename, dest_dir, queue, referer, headers: parse_headers(&headers)?, start: !paused })).await?;
            if let Response::Task(t) = r {
                print_task(&t);
            }
            Ok(())
        }
        Cmd::List => {
            if let Response::Tasks { tasks } = call(&Request::List).await? {
                if tasks.is_empty() {
                    println!("no downloads");
                }
                tasks.iter().for_each(print_task);
            }
            Ok(())
        }
        Cmd::Show { id } => {
            if let Response::Task(t) = call(&Request::Get { id }).await? {
                println!("{}", serde_json::to_string_pretty(&t)?);
            }
            Ok(())
        }
        Cmd::Pause { id } => call(&Request::Pause { id }).await.map(drop),
        Cmd::Resume { id } => call(&Request::Resume { id }).await.map(drop),
        Cmd::Rm { id, delete_files } => call(&Request::Remove { id, delete_files }).await.map(drop),
        Cmd::Queue { cmd } => match cmd {
            QueueCmd::List => {
                if let Response::Queues { queues } = call(&Request::QueueList).await? {
                    for q in queues {
                        let w = q.window.map_or("always".to_string(), |w| {
                            format!("{:02}:{:02}-{:02}:{:02} days={:07b}", w.start_min / 60, w.start_min % 60, w.end_min / 60, w.end_min % 60, w.days)
                        });
                        println!("{:<16} parallel={}  {}", q.name, q.max_parallel, w);
                    }
                }
                Ok(())
            }
            QueueCmd::Set { name, parallel, window, days } => {
                let window = match window {
                    Some(w) => {
                        let (s, e) = w.split_once('-').ok_or_else(|| anyhow::anyhow!("window must be HH:MM-HH:MM"))?;
                        let days = days.as_deref().map_or(Ok(0x7f), parse_days)?;
                        Some(Window { start_min: parse_hhmm(s)?, end_min: parse_hhmm(e)?, days })
                    }
                    None if days.is_some() => bail!("--days needs --window"),
                    None => None,
                };
                call(&Request::QueueSet(QueueInfo { name, max_parallel: parallel, window })).await.map(drop)
            }
            QueueCmd::Rm { name } => call(&Request::QueueRemove { name }).await.map(drop),
        },
        Cmd::Ping => {
            if let Response::Pong { version } = call(&Request::Ping).await? {
                println!("zdmd {version} is running");
            }
            Ok(())
        }
    }
}
