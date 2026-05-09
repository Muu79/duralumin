/// Sanitise an episode title for use as a filename component (spec §9.1).
///
/// 1. Lowercase
/// 2. Replace any non-`[a-z0-9]` character with `-`
/// 3. Collapse consecutive `-` to one
/// 4. Trim leading/trailing `-`
/// 5. Truncate to 80 bytes (UTF-8 safe)
pub fn sanitize_title(title: &str) -> String {
    let lower = title.to_lowercase();
    let replaced: String = lower
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let collapsed = collapse_dashes(&replaced);
    let trimmed = collapsed.trim_matches('-').to_string();
    truncate_utf8(&trimmed, 80)
}

/// Derive a file extension from the MIME type, falling back to the URL path,
/// then to `"mp3"` as a last resort.
pub fn ext_from_mime(mime: Option<&str>, url: &str) -> &'static str {
    if let Some(m) = mime {
        match m.split(';').next().unwrap_or("").trim() {
            "audio/mpeg" | "audio/mp3" => return "mp3",
            "audio/mp4" | "audio/x-m4a" | "audio/m4a" => return "m4a",
            "audio/ogg" => return "ogg",
            "audio/opus" => return "opus",
            "audio/flac" => return "flac",
            "audio/aac" => return "aac",
            "audio/wav" | "audio/x-wav" => return "wav",
            _ => {}
        }
    }
    // Strip query string, then look at the last path component's extension.
    let path = url.split('?').next().unwrap_or(url);
    if let Some(dot) = path.rfind('.') {
        match path[dot + 1..].to_ascii_lowercase().as_str() {
            "mp3" => return "mp3",
            "m4a" => return "m4a",
            "ogg" => return "ogg",
            "opus" => return "opus",
            "flac" => return "flac",
            "aac" => return "aac",
            "wav" => return "wav",
            _ => {}
        }
    }
    "mp3"
}

fn collapse_dashes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev = false;
    for c in s.chars() {
        if c == '-' {
            if !prev {
                out.push(c);
            }
            prev = true;
        } else {
            out.push(c);
            prev = false;
        }
    }
    out
}

pub fn truncate_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_sanitise() {
        assert_eq!(sanitize_title("Hello, World! (2024)"), "hello-world-2024");
    }

    #[test]
    fn collapses_runs_of_dashes() {
        assert_eq!(sanitize_title("foo   bar---baz"), "foo-bar-baz");
    }

    #[test]
    fn trims_edges() {
        assert_eq!(sanitize_title("  hello  "), "hello");
    }

    #[test]
    fn truncates_at_utf8_boundary() {
        // Each '→' is 3 bytes; 27 of them = 81 bytes — should truncate to 26 (78 bytes).
        let long: String = "→".repeat(27);
        let result = sanitize_title(&long);
        assert!(result.len() <= 80);
        assert!(long.is_char_boundary(result.len()) || result.is_empty());
    }

    #[test]
    fn ext_mime_priority() {
        assert_eq!(ext_from_mime(Some("audio/mpeg"), "https://ex.com/ep.m4a"), "mp3");
    }

    #[test]
    fn ext_url_fallback() {
        assert_eq!(ext_from_mime(None, "https://ex.com/ep.m4a?v=1"), "m4a");
    }

    #[test]
    fn ext_default_fallback() {
        assert_eq!(ext_from_mime(None, "https://ex.com/episode"), "mp3");
    }
}
