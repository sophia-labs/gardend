use crate::{
    document_service::BlockSnapshot,
    geist_song_lines::{parse_song_lines, song_verse_label},
    geist_song_store::LocalSongStore,
};

fn push_song_text_blocks(
    blocks: &mut Vec<BlockSnapshot>,
    block_id_prefix: &str,
    text: &str,
    order: &mut f64,
    voice_index: usize,
) {
    for (index, (line, has_break)) in parse_song_lines(text).into_iter().enumerate() {
        if !line.is_empty() {
            blocks.push(BlockSnapshot {
                id: format!("{block_id_prefix}-line-{voice_index}-{index}"),
                block_type: "paragraph".to_string(),
                content: line,
                parent_id: None,
                order: *order,
                level: None,
                checked: None,
                language: None,
                marks: Vec::new(),
            });
            *order += 1.0;
        }
        if has_break {
            blocks.push(BlockSnapshot {
                id: format!("{block_id_prefix}-break-{voice_index}-{index}"),
                block_type: "paragraph".to_string(),
                content: String::new(),
                parent_id: None,
                order: *order,
                level: None,
                checked: None,
                language: None,
                marks: Vec::new(),
            });
            *order += 1.0;
        }
    }
}

pub(crate) fn song_document_blocks(store: &LocalSongStore) -> Vec<BlockSnapshot> {
    let mut blocks = Vec::new();
    let mut order = 0.0;
    blocks.push(BlockSnapshot {
        id: "song-title".to_string(),
        block_type: "heading".to_string(),
        content: "The Song".to_string(),
        parent_id: None,
        order,
        level: Some(1),
        checked: None,
        language: None,
        marks: Vec::new(),
    });
    order += 1.0;
    for (index, verse) in store.verses.iter().enumerate() {
        blocks.push(BlockSnapshot {
            id: format!("song-verse-{index}"),
            block_type: "heading".to_string(),
            content: format!("Verse {}", song_verse_label(index)),
            parent_id: None,
            order,
            level: Some(2),
            checked: None,
            language: None,
            marks: Vec::new(),
        });
        order += 1.0;
        let prefix = format!("song-verse-{index}");
        push_song_text_blocks(&mut blocks, &prefix, &verse.text, &mut order, 0);
        for (counterpoint_index, counterpoint) in verse.counterpoints.iter().enumerate() {
            push_song_text_blocks(
                &mut blocks,
                &prefix,
                counterpoint,
                &mut order,
                counterpoint_index + 1,
            );
        }
    }
    if let Some(coda) = &store.coda {
        blocks.push(BlockSnapshot {
            id: "song-coda".to_string(),
            block_type: "heading".to_string(),
            content: format!("Coda ({} remaining)", coda.ejections_remaining),
            parent_id: None,
            order,
            level: Some(3),
            checked: None,
            language: None,
            marks: Vec::new(),
        });
        order += 1.0;
        push_song_text_blocks(&mut blocks, "song-coda", &coda.text, &mut order, 0);
    }
    blocks
}

pub(crate) fn song_archive_blocks(store: &LocalSongStore) -> Vec<BlockSnapshot> {
    let mut blocks = Vec::new();
    let mut order = 0.0;
    blocks.push(BlockSnapshot {
        id: "songs-archive-title".to_string(),
        block_type: "heading".to_string(),
        content: "Songs Archive".to_string(),
        parent_id: None,
        order,
        level: Some(1),
        checked: None,
        language: None,
        marks: Vec::new(),
    });
    order += 1.0;
    for (archive_index, archive) in store.archives.iter().enumerate() {
        blocks.push(BlockSnapshot {
            id: format!("songs-archive-{archive_index}"),
            block_type: "heading".to_string(),
            content: format!("Archived {}", archive.archived_at),
            parent_id: None,
            order,
            level: Some(3),
            checked: None,
            language: None,
            marks: Vec::new(),
        });
        order += 1.0;
        for (verse_index, verse) in archive.verses.iter().enumerate() {
            let prefix = format!("songs-archive-{archive_index}-{verse_index}");
            push_song_text_blocks(&mut blocks, &prefix, &verse.text, &mut order, 0);
            for (counterpoint_index, counterpoint) in verse.counterpoints.iter().enumerate() {
                push_song_text_blocks(
                    &mut blocks,
                    &prefix,
                    counterpoint,
                    &mut order,
                    counterpoint_index + 1,
                );
            }
        }
    }
    blocks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        clock::timestamp,
        geist_song_store::{LocalSongArchiveRecord, LocalSongVerse},
    };

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
    fn song_archive_blocks_include_archived_voices() {
        let mut store = test_song_store();
        store.archives.push(LocalSongArchiveRecord {
            archived_at: "1000".to_string(),
            verses: store.verses.clone(),
        });

        let blocks = song_archive_blocks(&store);

        assert!(blocks.iter().any(|block| block.id == "songs-archive-title"));
        assert!(blocks
            .iter()
            .any(|block| block.id == "songs-archive-0-0-line-0-0"));
        assert!(blocks
            .iter()
            .any(|block| block.id == "songs-archive-0-0-line-1-0"));
    }
}
