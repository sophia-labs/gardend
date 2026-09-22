use crate::json_utils::json_string;

pub(super) const YOUTUBE_NOTICE: &str = "YouTube transcript imports may have problems depending on caption availability, video restrictions, and network blocking.";

fn normalize_transcript_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(super) fn parse_youtube_transcript_rows(value: &serde_json::Value) -> Vec<(i64, String)> {
    let mut rows = Vec::new();
    let Some(events) = value.get("events").and_then(serde_json::Value::as_array) else {
        return rows;
    };
    for event in events {
        let start_sec = event
            .get("tStartMs")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(0)
            / 1000;
        let text = event
            .get("segs")
            .and_then(serde_json::Value::as_array)
            .map(|segments| {
                segments
                    .iter()
                    .filter_map(|segment| json_string(segment.get("utf8")))
                    .collect::<Vec<_>>()
                    .join("")
            })
            .unwrap_or_default();
        let normalized = normalize_transcript_text(&text);
        if !normalized.is_empty() {
            rows.push((start_sec.max(0), normalized));
        }
    }
    rows
}

fn youtube_timestamp(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let secs = seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{secs:02}")
    } else {
        format!("{minutes:02}:{secs:02}")
    }
}

fn youtube_sentence_ended(text: &str) -> bool {
    let trimmed = text
        .trim()
        .trim_end_matches(|ch: char| matches!(ch, '"' | '\'' | ')' | ']' | '}' | '”' | '’'));
    trimmed.ends_with(['.', '!', '?'])
}

fn youtube_materialize_chunk(rows: &[(i64, String)]) -> Option<(i64, i64, String)> {
    let first = rows.first()?;
    let last = rows.last()?;
    let text = normalize_transcript_text(
        &rows
            .iter()
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>()
            .join(" "),
    );
    if text.is_empty() {
        None
    } else {
        Some((first.0.max(0), last.0.max(0), text))
    }
}

fn youtube_readable_chunks(rows: &[(i64, String)], chunk_seconds: i64) -> Vec<(i64, i64, String)> {
    let window = chunk_seconds.max(5);
    let grace = 12_i64;
    let mut chunks = Vec::new();
    let mut current: Vec<(i64, String)> = Vec::new();
    let mut current_start: Option<i64> = None;
    for (start_sec, text) in rows {
        if current_start.is_none() {
            current_start = Some(*start_sec);
        }
        if !current.is_empty() {
            let elapsed = (*start_sec - current_start.unwrap_or(*start_sec)).max(0);
            let previous_text = current
                .last()
                .map(|(_, text)| text.as_str())
                .unwrap_or_default();
            if elapsed >= window
                && (youtube_sentence_ended(previous_text) || elapsed >= window + grace)
            {
                if let Some(chunk) = youtube_materialize_chunk(&current) {
                    chunks.push(chunk);
                }
                current.clear();
                current_start = Some(*start_sec);
            }
        }
        current.push((*start_sec, text.clone()));
    }
    if let Some(chunk) = youtube_materialize_chunk(&current) {
        chunks.push(chunk);
    }
    chunks
}

pub(super) fn build_youtube_transcript_markdown(
    title: &str,
    watch_url: &str,
    video_id: &str,
    rows: &[(i64, String)],
    mode: &str,
    chunk_seconds: i64,
    show_ranges: bool,
) -> String {
    let mut lines = vec![
        format!("## {title}"),
        String::new(),
        format!("Source: {watch_url}"),
        format!("Video ID: `{video_id}`"),
        String::new(),
        format!("> Note: {YOUTUBE_NOTICE}"),
        String::new(),
        "## Transcript".to_string(),
        String::new(),
    ];
    if mode.eq_ignore_ascii_case("timestamped") {
        for (start_sec, text) in rows {
            lines.push(format!(
                "- [{}]({watch_url}&t={}s) {text}",
                youtube_timestamp(*start_sec),
                start_sec
            ));
        }
        return lines.join("\n");
    }
    for (start_sec, end_sec, text) in youtube_readable_chunks(rows, chunk_seconds) {
        if show_ranges {
            lines.push(format!(
                "**[{}-{}]({watch_url}&t={}s)** {text}",
                youtube_timestamp(start_sec),
                youtube_timestamp(end_sec),
                start_sec
            ));
        } else {
            lines.push(text);
        }
        lines.push(String::new());
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_youtube_transcript_rows_joins_segments_and_skips_empty_events() {
        let payload = serde_json::json!({
            "events": [
                { "tStartMs": 1234, "segs": [{ "utf8": " Hello " }, { "utf8": "world\n" }] },
                { "tStartMs": -1000, "segs": [{ "utf8": "   " }] },
                { "tStartMs": 2500, "segs": [{ "utf8": "again" }] }
            ]
        });

        assert_eq!(
            parse_youtube_transcript_rows(&payload),
            vec![(1, "Hello world".to_string()), (2, "again".to_string())]
        );
    }

    #[test]
    fn timestamped_markdown_preserves_source_links_and_notice() {
        let markdown = build_youtube_transcript_markdown(
            "Demo",
            "https://www.youtube.com/watch?v=abc123abc12",
            "abc123abc12",
            &[(65, "Hello world".to_string())],
            "timestamped",
            30,
            false,
        );

        assert!(markdown.contains("## Demo"));
        assert!(markdown.contains(YOUTUBE_NOTICE));
        assert!(markdown
            .contains("- [01:05](https://www.youtube.com/watch?v=abc123abc12&t=65s) Hello world"));
    }

    #[test]
    fn readable_markdown_splits_on_sentence_boundaries_after_window() {
        let rows = vec![
            (0, "First thought.”".to_string()),
            (45, "Second thought continues".to_string()),
        ];
        let markdown = build_youtube_transcript_markdown(
            "Demo",
            "https://www.youtube.com/watch?v=abc123abc12",
            "abc123abc12",
            &rows,
            "readable",
            30,
            true,
        );

        assert!(markdown.contains(
            "**[00:00-00:00](https://www.youtube.com/watch?v=abc123abc12&t=0s)** First thought.”"
        ));
        assert!(markdown.contains("**[00:45-00:45](https://www.youtube.com/watch?v=abc123abc12&t=45s)** Second thought continues"));
    }
}
