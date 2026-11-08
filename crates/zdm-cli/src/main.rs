use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use zdm_core::{build_client, download_probed, probe, Options};
use zdm_proto::{call, AddRequest, AfterAll, GrabRequest, QueueInfo, Request, Response, TaskInfo, Window};

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
        #[arg(long)]
        user_agent: Option<String>,
        /// Extra header "Name: value" (repeatable), e.g. -H "Cookie: a=b"
        #[arg(short = 'H', long = "header")]
        headers: Vec<String>,
        /// Add paused instead of starting
        #[arg(long)]
        paused: bool,
        /// Sign-in for sites that ask, as NAME:PASSWORD (Basic or Digest; ftp:// too)
        #[arg(long)]
        user: Option<String>,
        /// Add it even if the same link is already in the list
        #[arg(long)]
        again: bool,
        /// For a video page: the id of a choice from `zdm formats` (with --filename for the saved name)
        #[arg(long)]
        format: Option<String>,
    },
    /// List what can be downloaded from a video page (YouTube and many other sites, through yt-dlp)
    Formats { url: String },
    /// Look for downloadable files on a web page
    Grab {
        url: String,
        /// Also follow links to other pages this many levels deep
        #[arg(long, default_value_t = 0)]
        depth: u32,
        /// File extensions to look for, comma separated (default: every file link)
        #[arg(long, value_delimiter = ',')]
        ext: Vec<String>,
        /// Also follow links to other sites
        #[arg(long)]
        any_site: bool,
        /// Only links matching this pattern (`*` = anything; repeatable)
        #[arg(long)]
        include: Vec<String>,
        /// Skip links matching this pattern (repeatable)
        #[arg(long)]
        exclude: Vec<String>,
    },
    /// Give a stopped or failed download a new address for the same file (keeps what was downloaded)
    Refresh { id: i64, url: String },
    /// Start a queued download before the others of its queue
    Top { id: i64 },
    /// Show or change the daemon's settings (connections, limits, proxy, folders)
    Settings {
        #[command(subcommand)]
        cmd: Option<SettingsCmd>,
    },
    /// What to do once all downloads are finished: nothing, sleep, shutdown, or run a command
    After {
        /// nothing | sleep | shutdown | command
        what: String,
        /// The command, for `command`
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
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
enum SettingsCmd {
    /// Connections per download (1-32)
    Connections { n: u32 },
    /// Speed limit per download in KiB/s (0 = none)
    Limit { kib: u64 },
    /// Speed limit for all downloads together in KiB/s (0 = none)
    GlobalLimit { kib: u64 },
    /// Automatic retries after network errors (0-20)
    Retries { n: u32 },
    /// Proxy address, or `none`
    Proxy { url: String },
    /// Parent folder of the category folders (absolute path)
    Folder { path: String },
    /// Shell command run after each download (file in $ZDM_FILE), or `none`
    PostCommand { command: String },
    /// When the file name is taken: rename | overwrite | skip
    OnExists { what: String },
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
        /// Speed limit shared by the queue's running downloads in KiB/s (0 = none)
        #[arg(long, default_value_t = 0)]
        limit_kib: u64,
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
        Cmd::Formats { url } => {
            if let Response::Formats(l) = call(&Request::Formats { url, cookie: None }).await? {
                println!("{}", l.title);
                for o in &l.options {
                    let size = o.size.map_or(String::new(), |s| format!("  ~{:.1} MB", s as f64 / 1e6));
                    println!("  {:<14} {:<9} {}{}", o.id, o.kind, o.label, size);
                }
            }
            Ok(())
        }
        Cmd::Add { url, filename, dest_dir, queue, referer, user_agent, headers, paused, user, again, format } => {
            let auth = user.map(|u| u.split_once(':').map_or((u.clone(), String::new()), |(a, b)| (a.to_string(), b.to_string())));
            let req = AddRequest { url, filename, dest_dir, queue, referer, user_agent, headers: parse_headers(&headers)?, start: !paused, allow_duplicate: again, auth, ytdlp: format };
            let r = call(&Request::Add(req)).await?;
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
                        let limit = if q.limit_kib > 0 { format!("  limit={} KiB/s", q.limit_kib) } else { String::new() };
                        println!("{:<16} parallel={}  {}{}", q.name, q.max_parallel, w, limit);
                    }
                }
                Ok(())
            }
            QueueCmd::Set { name, parallel, window, days, limit_kib } => {
                let window = match window {
                    Some(w) => {
                        let (s, e) = w.split_once('-').ok_or_else(|| anyhow::anyhow!("window must be HH:MM-HH:MM"))?;
                        let days = days.as_deref().map_or(Ok(0x7f), parse_days)?;
                        Some(Window { start_min: parse_hhmm(s)?, end_min: parse_hhmm(e)?, days })
                    }
                    None if days.is_some() => bail!("--days needs --window"),
                    None => None,
                };
                call(&Request::QueueSet(QueueInfo { name, max_parallel: parallel, window, limit_kib })).await.map(drop)
            }
            QueueCmd::Rm { name } => call(&Request::QueueRemove { name }).await.map(drop),
        },
        Cmd::Refresh { id, url } => call(&Request::SetUrl { id, url }).await.map(drop),
        Cmd::Top { id } => call(&Request::MoveToTop { id }).await.map(drop),
        Cmd::Grab { url, depth, ext, any_site, include, exclude } => {
            let req = GrabRequest { url, depth, same_host: !any_site, extensions: ext, max_pages: 0, include, exclude };
            if let Response::Links { links } = call(&Request::Grab(req)).await? {
                for l in &links {
                    println!("{}", l.url);
                }
                eprintln!("{} file link(s)", links.len());
            }
            Ok(())
        }
        Cmd::Settings { cmd } => {
            let Response::Settings(mut s) = call(&Request::GetSettings).await? else { bail!("unexpected answer") };
            match cmd {
                None => {
                    // passwords are not printed to the terminal
                    for l in &mut s.site_logins {
                        l.pass = "********".into();
                    }
                    println!("{}", serde_json::to_string_pretty(&s)?)
                }
                Some(c) => {
                    match c {
                        SettingsCmd::Connections { n } => s.connections = n,
                        SettingsCmd::Limit { kib } => s.speed_limit_kib = kib,
                        SettingsCmd::GlobalLimit { kib } => s.global_limit_kib = kib,
                        SettingsCmd::Retries { n } => s.auto_retries = n,
                        SettingsCmd::Proxy { url } => s.proxy = (url != "none").then_some(url),
                        SettingsCmd::Folder { path } => s.download_root = path,
                        SettingsCmd::OnExists { what } => s.on_exists = what,
                        SettingsCmd::PostCommand { command } => s.post_command = (!command.is_empty() && command != "none").then_some(command),
                    }
                    call(&Request::SetSettings(s)).await?;
                    println!("saved");
                }
            }
            Ok(())
        }
        Cmd::After { what, command } => {
            let a = match what.as_str() {
                "nothing" => AfterAll::Nothing,
                "sleep" => AfterAll::Sleep,
                "shutdown" => AfterAll::Shutdown,
                "command" => AfterAll::Command { command: command.join(" ") },
                other => bail!("unknown action {other}: use nothing, sleep, shutdown or command"),
            };
            call(&Request::SetAfterAll(a)).await.map(drop)
        }
        Cmd::Ping => {
            if let Response::Pong { version } = call(&Request::Ping).await? {
                println!("zdmd {version} is running");
            }
            Ok(())
        }
    }
}
