//! Text formatting for the task table (no GTK here, so it is unit-tested).

use zdm_proto::{State, TaskInfo};

pub fn bytes(n: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{n} B") } else { format!("{v:.1} {}", U[i]) }
}

pub fn eta(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs} sec"),
        60..=3599 => format!("{} min {} sec", secs / 60, secs % 60),
        _ => format!("{} h {} min", secs / 3600, (secs % 3600) / 60),
    }
}

/// Best name to show for a task: saved file name, else the server's name, else the link.
pub fn display_name(t: &TaskInfo) -> String {
    t.dest.as_deref().and_then(|d| std::path::Path::new(d).file_name()).map(|n| n.to_string_lossy().into_owned()).or_else(|| t.filename.clone()).unwrap_or_else(|| t.url.clone())
}

/// 0..=100 for the progress cell; unknown size shows 0.
pub fn percent(t: &TaskInfo) -> i32 {
    if t.state == State::Completed {
        return 100;
    }
    match t.size {
        Some(s) if s > 0 => ((t.downloaded as f64 * 100.0 / s as f64).floor() as i32).clamp(0, 100),
        _ => 0,
    }
}

/// Text drawn on the progress cell of the "Status" column.
pub fn status_label(t: &TaskInfo) -> String {
    match t.state {
        State::Completed => "Complete".into(),
        State::Failed => match t.error.as_deref() {
            // the daemon is about to try again by itself
            Some(e) if e.starts_with("Will retry") => "Waiting to retry".into(),
            e => format!("Error: {}", e.unwrap_or("unknown")),
        },
        State::Queued => "Queued".into(),
        State::Paused => match t.size {
            Some(s) if s > 0 => format!("Paused {:.1}%", t.downloaded as f64 * 100.0 / s as f64),
            _ => "Paused".into(),
        },
        State::Running => match t.size {
            Some(s) if s > 0 => format!("{:.1}%", t.downloaded as f64 * 100.0 / s as f64),
            _ => bytes(t.downloaded),
        },
    }
}

pub fn time_left(t: &TaskInfo, speed: f64) -> String {
    match (t.state, t.size) {
        (State::Running, Some(size)) if speed > 1.0 => eta((size.saturating_sub(t.downloaded) as f64 / speed) as u64),
        _ => String::new(),
    }
}

pub fn rate(t: &TaskInfo, speed: f64) -> String {
    if t.state == State::Running && speed > 1.0 { format!("{}/s", bytes(speed as u64)) } else { String::new() }
}

pub fn size(t: &TaskInfo) -> String {
    t.size.map(bytes).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(state: State, size: Option<u64>, done: u64) -> TaskInfo {
        TaskInfo { id: 1, url: "u".into(), filename: None, dest: None, category: None, queue: "Main".into(), state, size, downloaded: done, error: None, added_at: 0 }
    }

    #[test]
    fn sizes_and_times() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(1536), "1.5 KiB");
        assert_eq!(bytes(40 * 1024 * 1024), "40.0 MiB");
        assert_eq!(eta(5), "5 sec");
        assert_eq!(eta(125), "2 min 5 sec");
        assert_eq!(eta(7260), "2 h 1 min");
    }

    #[test]
    fn running_row() {
        let t = task(State::Running, Some(2 << 20), 1 << 20);
        assert_eq!(percent(&t), 50);
        assert_eq!(status_label(&t), "50.0%");
        assert_eq!(time_left(&t, (1 << 20) as f64), "1 sec");
        assert_eq!(rate(&t, (1 << 20) as f64), "1.0 MiB/s");
    }

    #[test]
    fn other_states() {
        assert_eq!(status_label(&task(State::Completed, Some(10), 10)), "Complete");
        assert_eq!(percent(&task(State::Completed, None, 0)), 100);
        assert_eq!(status_label(&task(State::Queued, None, 0)), "Queued");
        assert_eq!(status_label(&task(State::Paused, Some(200), 50)), "Paused 25.0%");
        let mut t = task(State::Failed, None, 0);
        t.error = Some("boom".into());
        assert_eq!(status_label(&t), "Error: boom");
        t.error = Some("Will retry (attempt 1 of 3) in 20s: connection refused".into());
        assert_eq!(status_label(&t), "Waiting to retry");
        assert_eq!(rate(&task(State::Paused, None, 0), 99.0), "");
    }
}
