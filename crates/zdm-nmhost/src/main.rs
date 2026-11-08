use std::io::{stdin, stdout};
use std::time::Duration;

use anyhow::{bail, Result};
use serde_json::{json, Value};
use tokio::io::BufReader;
use zdm_nmhost::install::{self, Action};
use zdm_nmhost::{err, ok, parse_message, read_frame, write_frame, Msg};
use zdm_proto::{connect, roundtrip, Io, Request, Response};

const USAGE: &str = "zdm-nmhost: browser <-> zdmd bridge

Usage:
  zdm-nmhost install   [--browser NAME]... [--chrome-extension-id ID]... [--dry-run]
  zdm-nmhost uninstall [--browser NAME]... [--dry-run]

Browsers start it themselves with no arguments (native messaging over stdin/stdout).
";

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("install") => manage(&args[1..], true),
        Some("uninstall") => manage(&args[1..], false),
        Some("--help" | "-h" | "help") => {
            print!("{USAGE}");
            Ok(())
        }
        // Browsers pass an origin / manifest path / --parent-window; none of that matters to us.
        _ => host().await,
    }
}

fn manage(args: &[String], installing: bool) -> Result<()> {
    let (mut only, mut ids, mut dry) = (Vec::new(), Vec::new(), false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--browser" => only.push(it.next().ok_or_else(|| anyhow::anyhow!("--browser needs a value"))?.clone()),
            "--chrome-extension-id" => ids.push(it.next().ok_or_else(|| anyhow::anyhow!("--chrome-extension-id needs a value"))?.clone()),
            "--dry-run" => dry = true,
            other => bail!("unknown argument {other}\n\n{USAGE}"),
        }
    }
    let exe = std::env::current_exe()?;
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
    let data = dirs::data_local_dir().unwrap_or_else(std::env::temp_dir).join("zdm");
    let actions: Vec<Action> = install::plan(&exe, &only, &ids, &home, &data)?;
    for a in &actions {
        println!("{} {}", if dry { "would" } else if installing { "install:" } else { "remove:" }, install::describe(a));
    }
    if !dry {
        if installing { install::apply(&actions)? } else { install::remove(&actions)? }
    }
    Ok(())
}

async fn host() -> Result<()> {
    let (mut inp, mut out) = (stdin().lock(), stdout().lock());
    while let Some(frame) = read_frame(&mut inp)? {
        let reply = handle(&frame).await;
        write_frame(&mut out, &serde_json::to_vec(&reply)?)?;
    }
    Ok(())
}

async fn handle(payload: &[u8]) -> Value {
    let v: Value = match serde_json::from_slice(payload) {
        Ok(v) => v,
        Err(_) => return err("invalid JSON"),
    };
    let req = match parse_message(&v) {
        Ok(Msg::Ping) => Request::Ping,
        Ok(Msg::Add(a)) => Request::Add(a),
        Ok(Msg::Formats { url }) => Request::Formats { url },
        Err(e) => return err(e),
    };
    match send(req).await {
        Ok(Response::Task(t)) => ok(json!({ "id": t.id, "state": t.state })),
        Ok(Response::Pong { version }) => ok(json!({ "version": version })),
        Ok(Response::Formats(list)) => ok(json!({ "title": list.title, "duration": list.duration, "options": list.options })),
        Ok(Response::Error { message }) => err(message),
        Ok(_) => err("unexpected response from zdmd"),
        Err(e) => err(format!("{e:#}")),
    }
}

/// Talk to the daemon, starting it first if it is not running.
async fn send(req: Request) -> Result<Response> {
    let mut s = BufReader::new(daemon().await?);
    roundtrip(&mut s, &req).await
}

async fn daemon() -> Result<Box<dyn Io>> {
    if let Ok(c) = connect().await {
        return Ok(c);
    }
    spawn_daemon()?;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Ok(c) = connect().await {
            return Ok(c);
        }
    }
    bail!("zdmd is not running and could not be started")
}

fn spawn_daemon() -> Result<()> {
    let name = if cfg!(windows) { "zdmd.exe" } else { "zdmd" };
    let sibling = std::env::current_exe()?.with_file_name(name);
    let mut cmd = std::process::Command::new(if sibling.exists() { sibling } else { name.into() });
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use std::os::windows::process::CommandExt;
        // CreateProcess inherits every inheritable handle. Our stdin/stdout are the browser's pipes; if the
        // daemon kept them, the browser would never see them close after the host exits.
        extern "system" {
            fn SetHandleInformation(handle: *mut std::ffi::c_void, mask: u32, flags: u32) -> i32;
        }
        const HANDLE_FLAG_INHERIT: u32 = 1;
        for h in [std::io::stdin().as_raw_handle(), std::io::stdout().as_raw_handle(), std::io::stderr().as_raw_handle()] {
            if !h.is_null() {
                // SAFETY: plain flag change on a handle we own; failure (e.g. no such handle) is harmless.
                unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0) };
            }
        }
        cmd.creation_flags(0x0800_0208); // CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0); // survive the browser closing the host
    }
    cmd.spawn().map(drop).map_err(|e| anyhow::anyhow!("cannot start {name}: {e}"))
}
