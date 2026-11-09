//! Per-connection layout of an unfinished download, read from the resume file the engine keeps next to
//! the partial file (`<dest>.part.zdm`). Pure parsing, unit-tested.

use serde::Deserialize;

#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct Segment {
    pub start: u64,
    pub end: u64,
    pub pos: u64,
}

#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct Layout {
    pub size: u64,
    pub segs: Vec<Segment>,
}

impl Layout {
    pub fn parse(bytes: &[u8]) -> Option<Layout> {
        serde_json::from_slice(bytes).ok()
    }

    /// Read the resume file of `dest`; `None` when there is none (not started yet, or already finished).
    pub fn read_for(dest: &str) -> Option<Layout> {
        Layout::parse(&std::fs::read(format!("{dest}.part.zdm")).ok()?)
    }

    /// Segments that still have bytes to fetch (roughly the number of open connections).
    #[allow(dead_code)]
    pub fn active(&self) -> usize {
        self.segs.iter().filter(|s| s.pos < s.end).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{"url":"http://x","size":100,"etag":"\"v1\"","last_modified":null,
        "segs":[{"start":0,"end":50,"pos":50},{"start":50,"end":100,"pos":70}]}"#;

    #[test]
    fn parses_engine_sidecar() {
        let l = Layout::parse(SAMPLE.as_bytes()).unwrap();
        assert_eq!(l.size, 100);
        assert_eq!(l.segs.len(), 2);
        assert_eq!(l.active(), 1);
    }

    #[test]
    fn garbage_is_none() {
        assert!(Layout::parse(b"nope").is_none());
        assert!(Layout::read_for("/definitely/not/here").is_none());
    }
}
