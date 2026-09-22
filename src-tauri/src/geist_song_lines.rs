pub(crate) fn song_verse_label(index: usize) -> String {
    if index == 0 {
        "0".to_string()
    } else {
        format!("-{index}")
    }
}

pub(crate) fn parse_song_lines(text: &str) -> Vec<(String, bool)> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            if let Some(stripped) = line.strip_suffix('/') {
                (stripped.trim_end().to_string(), true)
            } else {
                (line.to_string(), false)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn song_lines_preserve_explicit_break_markers() {
        assert_eq!(
            parse_song_lines("first /\n\nsecond\nthird/"),
            vec![
                ("first".to_string(), true),
                ("second".to_string(), false),
                ("third".to_string(), true),
            ]
        );
    }
}
