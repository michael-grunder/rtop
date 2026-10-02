//! Small string helpers shared by the poller and the renderers.

/// Truncates to at most `max_chars` characters, appending `ellipsis` when
/// anything was cut.
pub fn truncate_chars(input: &str, max_chars: usize, ellipsis: &str) -> String {
    match input.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => format!("{}{ellipsis}", &input[..byte_idx]),
        None => input.to_string(),
    }
}

/// First line of `input`, trimmed and truncated to `max_chars` characters.
pub fn first_line(input: &str, max_chars: usize) -> String {
    truncate_chars(input.lines().next().unwrap_or(input).trim(), max_chars, "")
}

#[cfg(test)]
mod tests {
    use super::{first_line, truncate_chars};

    #[test]
    fn truncates_on_char_boundaries() {
        assert_eq!(truncate_chars("héllo", 2, "..."), "hé...");
        assert_eq!(truncate_chars("héllo", 5, "..."), "héllo");
        assert_eq!(truncate_chars("", 0, "..."), "");
        assert_eq!(first_line("  abc def \nsecond", 3), "abc");
    }
}
