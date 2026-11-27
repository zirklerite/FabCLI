use serde_json::Value;

pub fn print_json(value: &Value, pretty: bool) {
    let s = if pretty {
        serde_json::to_string_pretty(value)
    } else {
        serde_json::to_string(value)
    }
    .expect("serializing a constructed json::Value cannot fail");
    println!("{}", s);
}

/// The first `max` characters of `s`, cut on a char boundary, and
/// whether anything was cut. Byte slicing (`&s[..n]`) panics when byte
/// `n` falls inside a multi-byte character, which localized upstream
/// messages can trigger.
pub fn truncate_chars(s: &str, max: usize) -> (&str, bool) {
    match s.char_indices().nth(max) {
        Some((idx, _)) => (&s[..idx], true),
        None => (s, false),
    }
}

/// A response body cut to `max` characters for an error message or a
/// log line, with `…(+N bytes)` saying how much was left out.
pub fn preview(body: &str, max: usize) -> String {
    match truncate_chars(body, max) {
        (head, true) => format!("{head}…(+{} bytes)", body.len() - head.len()),
        (head, false) => head.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{preview, truncate_chars};

    #[test]
    fn preview_reports_what_was_cut() {
        assert_eq!(preview("abcdef", 4), "abcd…(+2 bytes)");
        assert_eq!(preview("abc", 4), "abc");
    }

    #[test]
    fn truncate_chars_is_char_boundary_safe() {
        // 3-byte characters: a byte cut at 4 would land mid-character.
        let s = "日本語のエラー";
        assert_eq!(truncate_chars(s, 2), ("日本", true));
        assert_eq!(truncate_chars(s, 7), (s, false));
        assert_eq!(truncate_chars("abc", 10), ("abc", false));
        assert_eq!(truncate_chars("", 0), ("", false));
    }
}
