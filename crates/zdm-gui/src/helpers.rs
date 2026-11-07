//! Small pure helpers for the dialogs (unit-tested, no GTK).

use zdm_proto::Window;

/// URLs from pasted text: one per line (or whitespace separated), http/https only, duplicates dropped.
pub fn parse_urls(text: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for tok in text.split_whitespace() {
        let t = tok.trim_matches(|c| matches!(c, '<' | '>' | '"' | '\'' | ',' | ';'));
        if (t.starts_with("http://") || t.starts_with("https://")) && t.len() > 8 && !out.iter().any(|u| u == t) {
            out.push(t.to_string());
        }
    }
    out
}

pub const DAY_NAMES: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// `days` bit 0 = Monday ... bit 6 = Sunday (same as the daemon).
pub fn days_label(days: u8) -> String {
    if days & 0x7f == 0x7f {
        return "every day".into();
    }
    let v: Vec<&str> = (0..7).filter(|d| days & (1 << d) != 0).map(|d| DAY_NAMES[d]).collect();
    if v.is_empty() { "no days".into() } else { v.join(", ") }
}

pub fn hm(min: u32) -> (u32, u32) {
    ((min / 60).min(23), min % 60)
}

pub fn minutes(h: u32, m: u32) -> u32 {
    (h.min(23)) * 60 + m.min(59)
}

/// One-line description of a queue's schedule for the sidebar/tooltips.
pub fn window_label(w: &Option<Window>) -> String {
    match w {
        None => "runs any time".into(),
        Some(w) => {
            let (sh, sm) = hm(w.start_min);
            let (eh, em) = hm(w.end_min);
            format!("{sh:02}:{sm:02}–{eh:02}:{em:02}, {}", days_label(w.days))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_extracted_and_deduped() {
        let t = "https://a.org/x.zip\nnot a url\n<http://b.net/y> ftp://c/z\nhttps://a.org/x.zip, https://";
        assert_eq!(parse_urls(t), vec!["https://a.org/x.zip", "http://b.net/y"]);
        assert!(parse_urls("").is_empty());
    }

    #[test]
    fn day_and_time_labels() {
        assert_eq!(days_label(0x7f), "every day");
        assert_eq!(days_label(0b0011111), "Mon, Tue, Wed, Thu, Fri");
        assert_eq!(days_label(0), "no days");
        assert_eq!(hm(23 * 60 + 5), (23, 5));
        assert_eq!(minutes(7, 30), 450);
        assert_eq!(window_label(&None), "runs any time");
        assert_eq!(window_label(&Some(Window { start_min: 60, end_min: 420, days: 0x7f })), "01:00–07:00, every day");
    }
}
