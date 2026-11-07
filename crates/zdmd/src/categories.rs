//! File-type categories: a file's extension picks its download sub-folder. The built-in table is in
//! `zdm-proto` (the window shows it in Options); the user can replace the file types of any category.

use std::collections::BTreeMap;

use zdm_proto::{CATEGORY_TABLE, OTHER_CATEGORY};

pub const OTHER: &str = OTHER_CATEGORY;

/// Category for a file name using only the built-in table.
pub fn categorize(filename: &str) -> String {
    categorize_with(filename, &BTreeMap::new())
}

/// Category for a file name, by extension (case-insensitive). A category listed in `overrides` uses its own
/// list of file types instead of the built-in one, and those types win over the built-in table.
pub fn categorize_with(filename: &str, overrides: &BTreeMap<String, Vec<String>>) -> String {
    let ext = match filename.rsplit_once('.') {
        Some((stem, e)) if !stem.is_empty() => e.to_ascii_lowercase(),
        _ => return OTHER.to_string(),
    };
    if let Some((cat, _)) = overrides.iter().find(|(_, exts)| exts.iter().any(|e| e.eq_ignore_ascii_case(&ext))) {
        return cat.clone();
    }
    CATEGORY_TABLE
        .iter()
        .filter(|(c, _)| !overrides.contains_key(*c))
        .find(|(_, exts)| exts.contains(&ext.as_str()))
        .map_or(OTHER, |(c, _)| c)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_by_extension() {
        assert_eq!(categorize("a.ZIP"), "Compressed");
        assert_eq!(categorize("movie.mkv"), "Video");
        assert_eq!(categorize("noext"), "Other");
        assert_eq!(categorize("weird.unknownext"), "Other");
        assert_eq!(categorize("archive.tar.gz"), "Compressed");
    }

    #[test]
    fn user_file_types_replace_and_win() {
        let mut o = BTreeMap::new();
        o.insert("Video".to_string(), vec!["mp4".to_string(), "xyz".to_string()]);
        o.insert("Documents".to_string(), vec!["mkv".to_string()]);
        assert_eq!(categorize_with("a.xyz", &o), "Video", "a new type");
        assert_eq!(categorize_with("a.avi", &o), "Other", "no longer in the replaced Video list");
        assert_eq!(categorize_with("a.MKV", &o), "Documents", "moved to another category");
        assert_eq!(categorize_with("a.zip", &o), "Compressed", "untouched categories keep their list");
    }
}
