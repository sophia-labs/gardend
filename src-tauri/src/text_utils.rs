pub(crate) fn compact_text(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn truncate_preview_chars(value: &str, max_chars: usize) -> String {
    let mut result = value.chars().take(max_chars).collect::<String>();
    if value.chars().count() > max_chars {
        result.push_str("...");
    }
    result
}

pub(crate) fn text_preview(value: &str, max_chars: usize) -> String {
    truncate_preview_chars(&compact_text(value), max_chars)
}

pub(crate) fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_text_collapses_all_whitespace() {
        assert_eq!(compact_text(" alpha\n\t beta   gamma "), "alpha beta gamma");
    }

    #[test]
    fn text_preview_compacts_and_adds_ellipsis_when_truncated() {
        assert_eq!(text_preview(" alpha\n beta gamma ", 10), "alpha beta...");
        assert_eq!(text_preview("short text", 20), "short text");
    }

    #[test]
    fn truncate_chars_respects_unicode_boundaries() {
        assert_eq!(truncate_chars("éclair", 2), "éc");
    }
}
