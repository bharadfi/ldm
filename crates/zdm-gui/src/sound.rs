//! Event sounds (download finished / failed / everything done). Plays a file with whatever small player the desktop
//! has (`pw-play`, `paplay`, `canberra-gtk-play`, `ffplay`); the defaults are files of the freedesktop sound theme.

use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SoundEvent {
    Complete,
    Failed,
    AllDone,
}

/// A saved choice: empty = no sound, `default` = the theme's sound for the event, else a file path.
pub fn resolve(setting: &str, ev: SoundEvent) -> Option<String> {
    match setting.trim() {
        "" => None,
        "default" => Some(default_file(ev).to_string()),
        path => Some(path.to_string()),
    }
}

pub fn default_file(ev: SoundEvent) -> &'static str {
    match ev {
        SoundEvent::Complete => "/usr/share/sounds/freedesktop/stereo/complete.oga",
        SoundEvent::Failed => "/usr/share/sounds/freedesktop/stereo/dialog-error.oga",
        SoundEvent::AllDone => "/usr/share/sounds/freedesktop/stereo/message.oga",
    }
}

/// The first available player and its arguments.
pub fn command_for(file: &str, path_dirs: &[PathBuf]) -> Option<(PathBuf, Vec<String>)> {
    let players: [(&str, Vec<String>); 4] = [
        ("pw-play", vec![file.into()]),
        ("paplay", vec![file.into()]),
        ("canberra-gtk-play", vec!["-f".into(), file.into()]),
        ("ffplay", vec!["-nodisp".into(), "-autoexit".into(), "-loglevel".into(), "quiet".into(), file.into()]),
    ];
    players.into_iter().find_map(|(name, args)| path_dirs.iter().map(|d| d.join(name)).find(|p| p.is_file()).map(|p| (p, args)))
}

/// Play `file` in the background; silently does nothing when there is no player or no such file.
pub fn play(file: &str) {
    if !std::path::Path::new(file).is_file() {
        return;
    }
    let dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    if let Some((prog, args)) = command_for(file, &dirs) {
        let _ = std::process::Command::new(prog).args(args).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn();
    }
}

/// Everything just finished: it was busy on the last snapshot and is idle now (and something existed).
pub fn all_done(was_busy: bool, busy_now: bool) -> bool {
    was_busy && !busy_now
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_resolve_to_files() {
        assert_eq!(resolve("", SoundEvent::Complete), None);
        assert_eq!(resolve("  ", SoundEvent::Complete), None);
        assert_eq!(resolve("default", SoundEvent::Failed).as_deref(), Some("/usr/share/sounds/freedesktop/stereo/dialog-error.oga"));
        assert_eq!(resolve("/home/me/ding.wav", SoundEvent::AllDone).as_deref(), Some("/home/me/ding.wav"));
    }

    #[test]
    fn first_available_player_is_used() {
        let d = tempfile::tempdir().unwrap();
        assert!(command_for("/a.oga", &[d.path().to_path_buf()]).is_none());
        std::fs::write(d.path().join("paplay"), "").unwrap();
        std::fs::write(d.path().join("canberra-gtk-play"), "").unwrap();
        let (p, args) = command_for("/a.oga", &[d.path().to_path_buf()]).unwrap();
        assert!(p.ends_with("paplay") && args == ["/a.oga"], "{p:?} {args:?}");
        std::fs::write(d.path().join("pw-play"), "").unwrap();
        assert!(command_for("/a.oga", &[d.path().to_path_buf()]).unwrap().0.ends_with("pw-play"));
    }

    #[test]
    fn done_means_busy_then_idle() {
        assert!(all_done(true, false));
        assert!(!all_done(false, false) && !all_done(true, true) && !all_done(false, true));
    }
}
