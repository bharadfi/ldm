//! File-type categories: a file's extension picks its download sub-folder.

const TABLE: &[(&str, &[&str])] = &[
    ("Compressed", &["zip", "7z", "rar", "gz", "bz2", "xz", "zst", "tar", "tgz", "z", "cab", "lzh", "arj"]),
    ("Documents", &["pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp", "txt", "epub", "rtf", "csv"]),
    ("Music", &["mp3", "flac", "aac", "ogg", "opus", "wav", "wma", "m4a", "mpa"]),
    ("Video", &["mp4", "mkv", "avi", "mov", "wmv", "webm", "flv", "m4v", "mpg", "mpeg", "3gp", "ogv", "ts"]),
    ("Programs", &["exe", "msi", "deb", "rpm", "appimage", "apk", "iso", "img", "bin", "sh", "run", "flatpakref", "snap"]),
    ("Images", &["jpg", "jpeg", "png", "gif", "bmp", "webp", "svg", "tif", "tiff", "ico"]),
];

pub const OTHER: &str = "Other";

/// Category for a file name, by extension (case-insensitive).
pub fn categorize(filename: &str) -> &'static str {
    let ext = match filename.rsplit_once('.') {
        Some((_, e)) => e.to_ascii_lowercase(),
        None => return OTHER,
    };
    TABLE.iter().find(|(_, exts)| exts.contains(&ext.as_str())).map_or(OTHER, |(c, _)| c)
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
}
