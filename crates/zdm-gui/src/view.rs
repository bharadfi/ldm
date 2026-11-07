//! Which tasks a sidebar entry shows (pure logic, unit-tested).

use zdm_proto::State;

pub const CATEGORIES: [&str; 7] = ["Compressed", "Documents", "Music", "Video", "Programs", "Images", "Other"];

/// Full-colour theme icon for a category row in the sidebar.
pub fn category_icon(c: &str) -> &'static str {
    match c {
        "Compressed" => "package-x-generic",
        "Documents" => "x-office-document",
        "Music" => "audio-x-generic",
        "Video" => "video-x-generic",
        "Programs" => "application-x-executable",
        "Images" => "image-x-generic",
        _ => "text-x-generic",
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Section {
    All,
    Category(String),
    Unfinished,
    Finished,
    Queue(String),
}

impl Section {
    /// Sidebar rows carry a string key such as `cat:Video` or `q:Main`.
    pub fn parse(key: &str) -> Section {
        match key {
            "unfinished" => Section::Unfinished,
            "finished" => Section::Finished,
            k if k.starts_with("cat:") => Section::Category(k[4..].into()),
            k if k.starts_with("q:") => Section::Queue(k[2..].into()),
            _ => Section::All,
        }
    }

    pub fn matches(&self, state: State, category: &str, queue: &str) -> bool {
        match self {
            Section::All => true,
            Section::Category(c) => c == category,
            Section::Unfinished => state != State::Completed,
            Section::Finished => state == State::Completed,
            Section::Queue(q) => q == queue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip() {
        assert_eq!(Section::parse("all"), Section::All);
        assert_eq!(Section::parse("cat:Video"), Section::Category("Video".into()));
        assert_eq!(Section::parse("q:Night"), Section::Queue("Night".into()));
        assert_eq!(Section::parse("finished"), Section::Finished);
    }

    #[test]
    fn filtering() {
        assert!(Section::Unfinished.matches(State::Paused, "Video", "Main"));
        assert!(!Section::Unfinished.matches(State::Completed, "Video", "Main"));
        assert!(Section::Finished.matches(State::Completed, "Video", "Main"));
        assert!(Section::Category("Video".into()).matches(State::Running, "Video", "Main"));
        assert!(!Section::Queue("Night".into()).matches(State::Running, "Video", "Main"));
    }
}
