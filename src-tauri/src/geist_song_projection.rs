pub(crate) use crate::geist_song_blocks::{song_archive_blocks, song_document_blocks};
pub(crate) use crate::geist_song_lines::song_verse_label;

use crate::{
    geist_song_lines::parse_song_lines,
    geist_song_store::{LocalSongCoda, LocalSongStore, LocalSongVerse},
};

fn render_song_voice_line(line: &str, voice_index: usize) -> String {
    match voice_index {
        0 => line.to_string(),
        1 => format!("_{line}_"),
        _ => format!("**_{line}_**"),
    }
}

fn song_interleaved_markdown(verse: &LocalSongVerse) -> Vec<String> {
    let original_lines = parse_song_lines(&verse.text);
    let counterpoint_lines = verse
        .counterpoints
        .iter()
        .map(|counterpoint| parse_song_lines(counterpoint))
        .collect::<Vec<_>>();
    let max_lines = std::iter::once(original_lines.len())
        .chain(counterpoint_lines.iter().map(Vec::len))
        .max()
        .unwrap_or(0);
    let mut rendered = Vec::new();
    for index in 0..max_lines {
        if let Some((line, has_break)) = original_lines.get(index) {
            if !line.is_empty() {
                rendered.push(render_song_voice_line(line, 0));
            }
            if *has_break {
                rendered.push(String::new());
            }
        }
        for (voice_index, lines) in counterpoint_lines.iter().enumerate() {
            if let Some((line, has_break)) = lines.get(index) {
                if !line.is_empty() {
                    rendered.push(render_song_voice_line(line, voice_index + 1));
                }
                if *has_break {
                    rendered.push(String::new());
                }
            }
        }
    }
    rendered
}

fn render_song_verse_markdown(verse: &LocalSongVerse, index: usize, include_title: bool) -> String {
    let mut lines = Vec::new();
    if include_title {
        lines.push("# The Song".to_string());
        lines.push(String::new());
    }
    lines.push(format!("## Verse {}", song_verse_label(index)));
    lines.push(String::new());
    lines.extend(song_interleaved_markdown(verse));
    lines.join("\n").trim().to_string()
}

fn render_song_coda_markdown(coda: &LocalSongCoda) -> String {
    let mut lines = vec![
        format!("### Coda ({} remaining)", coda.ejections_remaining),
        String::new(),
    ];
    lines.extend(
        coda.text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(|line| format!("_{line}_")),
    );
    lines.join("\n").trim().to_string()
}

pub(crate) fn song_music_json(store: &LocalSongStore) -> serde_json::Value {
    let mut sections = store
        .verses
        .iter()
        .enumerate()
        .map(|(index, verse)| render_song_verse_markdown(verse, index, index == 0))
        .filter(|section| !section.trim().is_empty())
        .collect::<Vec<_>>();
    if let Some(coda) = &store.coda {
        if !coda.text.trim().is_empty() {
            sections.push(render_song_coda_markdown(coda));
        }
    }
    let section_count = sections.len();
    let counterpoint_parts = store
        .verses
        .iter()
        .map(|verse| verse.counterpoints.len())
        .collect::<Vec<_>>();
    let mut result = serde_json::json!({
        "verses": sections,
        "verse_count": section_count,
        "verseCount": section_count,
        "active_verse_count": store.verses.len(),
        "activeVerseCount": store.verses.len(),
        "source": "local-song-store",
    });
    if counterpoint_parts.iter().any(|count| *count > 0) {
        result["counterpoint_parts"] = serde_json::json!(counterpoint_parts);
        result["counterpointParts"] = result["counterpoint_parts"].clone();
    }
    if let Some(coda) = &store.coda {
        result["coda"] = serde_json::json!(coda.text);
        result["coda_ejections_remaining"] = serde_json::json!(coda.ejections_remaining);
        result["codaEjectionsRemaining"] = result["coda_ejections_remaining"].clone();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::timestamp;

    fn test_song_store() -> LocalSongStore {
        LocalSongStore {
            schema_version: 1,
            graph_id: "graph-a".to_string(),
            observer: String::new(),
            verses: vec![LocalSongVerse {
                text: "alpha /\nbeta".to_string(),
                counterpoints: vec!["counter".to_string()],
                created_at: timestamp(),
                updated_at: timestamp(),
            }],
            coda: None,
            archives: Vec::new(),
        }
    }

    #[test]
    fn song_music_json_reports_active_and_rendered_sections() {
        let mut store = test_song_store();
        store.coda = Some(LocalSongCoda {
            text: "closing".to_string(),
            ejections_remaining: 2,
            created_at: "1000".to_string(),
        });

        let value = song_music_json(&store);
        assert_eq!(
            value.get("verse_count").and_then(serde_json::Value::as_u64),
            Some(2)
        );
        assert_eq!(
            value
                .get("active_verse_count")
                .and_then(serde_json::Value::as_u64),
            Some(1)
        );
        assert_eq!(
            value
                .get("counterpoint_parts")
                .and_then(serde_json::Value::as_array)
                .and_then(|values| values.first())
                .and_then(serde_json::Value::as_u64),
            Some(1)
        );
        assert_eq!(
            value
                .get("coda_ejections_remaining")
                .and_then(serde_json::Value::as_i64),
            Some(2)
        );
    }
}
