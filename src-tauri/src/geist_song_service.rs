use crate::app_runtime::AppHandle;
use crate::{
    clock::timestamp,
    geist_song_store::{
        past_songs_doc_id, persist_song_store, read_song_store, read_song_store_for,
        song_music_json, LocalSongArchiveRecord, LocalSongCoda, LocalSongVerse,
        CODA_EJECTION_LIFETIME, MAX_SONG_VERSES, MAX_SONG_VOICES,
    },
    graph_service::touch_graph_updated_at,
    mcp_utils::{mcp_arg_string, mcp_graph_id_or_default},
    paths::existing_graph_dir,
    profile_service::touch_profile_updated_at,
};

pub(super) fn mcp_local_song_summary(app: &AppHandle, graph_id: &str) -> serde_json::Value {
    existing_graph_dir(app, graph_id)
        .and_then(|graph_dir| read_song_store(&graph_dir, graph_id))
        .map(|store| song_music_json(&store))
        .unwrap_or_else(|error| {
            serde_json::json!({
                "verses": [],
                "verse_count": 0,
                "verseCount": 0,
                "source": "local-song-store",
                "error": error,
            })
        })
}

pub(super) fn mcp_local_music(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    // Per-observer Song (fix #3): route by the wire-supplied observer; empty/absent
    // ⇒ the shared singleton (today's behavior, byte-identical store + graph).
    let observer =
        mcp_arg_string(arguments, &["observer_agent_id", "observerAgentId"]).unwrap_or_default();
    let store = read_song_store_for(&graph_dir, &graph_id, &observer)?;
    // `music` is a memory.read operation. Rendering a Song must not rewrite its
    // source, rebuild its document/RDF projections, or mark the graph edited.
    // A missing Song is rendered from the in-memory default; `sing` remains the
    // explicit write/materialization path. Graph lookup's existing bootstrap
    // behavior is a separate contract, not made read-only by this change.
    Ok(song_music_json(&store))
}

#[cfg(all(test, feature = "headless", not(feature = "desktop")))]
mod music_read_tests {
    use super::*;
    use crate::{
        graph_service::{create_graph_service, CreateGraphInput},
        paths::profile_dir,
    };
    use std::{collections::BTreeMap, path::Path};

    fn files_except(root: &Path, excluded: Option<&Path>) -> BTreeMap<String, Vec<u8>> {
        fn visit(
            root: &Path,
            at: &Path,
            excluded: Option<&Path>,
            files: &mut BTreeMap<String, Vec<u8>>,
        ) {
            for entry in std::fs::read_dir(at).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if excluded == Some(path.as_path()) {
                    continue;
                }
                if entry.file_type().unwrap().is_dir() {
                    visit(root, &path, excluded, files);
                } else {
                    files.insert(
                        path.strip_prefix(root)
                            .unwrap()
                            .to_string_lossy()
                            .into_owned(),
                        std::fs::read(&path).unwrap(),
                    );
                }
            }
        }
        let mut files = BTreeMap::new();
        visit(root, root, excluded, &mut files);
        files
    }

    fn files_at(root: &Path) -> BTreeMap<String, Vec<u8>> {
        files_except(root, None)
    }

    fn assert_files_unchanged(root: &Path, before: &BTreeMap<String, Vec<u8>>) {
        let after = files_at(root);
        assert_file_maps_unchanged(before, &after);
    }

    fn assert_file_maps_unchanged(
        before: &BTreeMap<String, Vec<u8>>,
        after: &BTreeMap<String, Vec<u8>>,
    ) {
        // Compare full bytes but never dump an entire graph in failure output.
        let changed = before
            .keys()
            .chain(after.keys())
            .filter(|key| before.get(*key) != after.get(*key))
            .collect::<std::collections::BTreeSet<_>>();
        assert!(
            changed.is_empty(),
            "Song read changed profile files: {changed:?}"
        );
    }

    fn logical_quads(store: &oxigraph::store::Store) -> std::collections::BTreeSet<String> {
        store
            .iter()
            .map(|quad| quad.unwrap().to_string())
            .collect()
    }

    fn assert_logical_quads_unchanged(
        store: &oxigraph::store::Store,
        before: &std::collections::BTreeSet<String>,
    ) {
        assert!(
            &logical_quads(store) == before,
            "Song read changed logical RDF quads"
        );
    }

    fn with_graph(check: impl FnOnce(AppHandle, std::path::PathBuf)) {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let profile =
            std::env::temp_dir().join(format!("garden-music-read-{}", uuid::Uuid::new_v4()));
        let previous = std::env::var_os("GARDEN_PROFILE_DIR");
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            create_graph_service(
                &app,
                CreateGraphInput {
                    graph_id: Some("song-read".to_string()),
                    title: "Song read lab".to_string(),
                    description: None,
                    operation_id: None,
                },
            )
            .unwrap();
            assert_eq!(profile_dir(&app).unwrap(), profile);
            check(app, profile.clone());
        }));
        match previous {
            Some(value) => std::env::set_var("GARDEN_PROFILE_DIR", value),
            None => std::env::remove_var("GARDEN_PROFILE_DIR"),
        }
        // Only this test's freshly generated, exact profile is disposable.
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(error) = result {
            std::panic::resume_unwind(error);
        }
    }

    #[test]
    fn music_read_renders_default_without_persisting_it() {
        with_graph(|app, profile| {
            let before = files_at(&profile);
            let value = mcp_local_music(app, &serde_json::json!({"graphId":"song-read"})).unwrap();
            assert!(value["activeVerseCount"].as_u64().unwrap() > 0);
            assert_files_unchanged(&profile, &before);
        });
    }

    #[test]
    fn music_read_preserves_existing_observer_source_without_projection() {
        with_graph(|app, profile| {
            let graph = existing_graph_dir(&app, "song-read").unwrap();
            let narrative = graph.join("narrative").join("readiness");
            std::fs::create_dir_all(&narrative).unwrap();
            // Old, source-only Song deliberately has no document/RDF projection.
            let source = br#"{"schemaVersion":1,"graphId":"song-read","observer":"readiness","verses":[{"text":"Leave room for the next reader.","counterpoints":[],"createdAt":"1","updatedAt":"1"}],"archives":[]}"#;
            std::fs::write(narrative.join("song.json"), source).unwrap();
            let before = files_at(&profile);
            let value = mcp_local_music(
                app,
                &serde_json::json!({"graphId":"song-read","observerAgentId":"readiness"}),
            )
            .unwrap();
            assert!(value["verses"][0]
                .as_str()
                .unwrap()
                .contains("Leave room for the next reader."));
            assert_files_unchanged(&profile, &before);
        });
    }

    #[test]
    fn explicit_sing_still_persists_a_song_that_music_can_read() {
        with_graph(|app, profile| {
            mcp_local_sing(
                app.clone(),
                &serde_json::json!({
                    "graphId":"song-read", "verse":"Make the next return easier."
                }),
            )
            .unwrap();
            let graph = existing_graph_dir(&app, "song-read").unwrap();
            assert!(graph.join("narrative/song.json").is_file());
            assert!(graph.join("documents/geist-song/document.json").is_file());
            // This case has just written a live RocksDB store. Its MANIFEST and
            // table files may change independently of the logical Song read.
            // Keep exact bytes everywhere else, and compare the complete RDF
            // quad set rather than treating the database layout as immutable.
            let database = graph.join("store.oxigraph");
            assert!(database.is_dir());
            let store = crate::rdf_store_service::open_graph_store(&graph).unwrap();
            let before = files_except(&profile, Some(&database));
            let quads_before = logical_quads(&store);
            assert!(!quads_before.is_empty());
            let result =
                mcp_local_music(app.clone(), &serde_json::json!({"graphId":"song-read"})).unwrap();
            assert!(result["verses"][0]
                .as_str()
                .unwrap()
                .contains("Make the next return easier."));
            assert!(database.is_dir());
            assert_file_maps_unchanged(&before, &files_except(&profile, Some(&database)));
            assert_logical_quads_unchanged(&store, &quads_before);

            // Discriminator: ignoring physical database bytes must not hide a
            // real RDF write. Exercise the same assertion against one.
            use oxigraph::model::{GraphName, Literal, NamedNode, Quad};
            store
                .insert(&Quad::new(
                    NamedNode::new("urn:test:song-read:mutation").unwrap(),
                    NamedNode::new("urn:test:song-read:predicate").unwrap(),
                    Literal::new_simple_literal("must be detected"),
                    GraphName::DefaultGraph,
                ))
                .unwrap();
            assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                assert_logical_quads_unchanged(&store, &quads_before);
            }))
            .is_err());
        });
    }
}

fn mcp_song_verse_index(arguments: &serde_json::Value) -> Option<i64> {
    ["verse_index", "verseIndex"]
        .iter()
        .find_map(|key| arguments.get(*key).and_then(serde_json::Value::as_i64))
}

pub(super) fn mcp_local_sing(
    app: AppHandle,
    arguments: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    let graph_id = mcp_graph_id_or_default(&app, arguments)?;
    let graph_dir = existing_graph_dir(&app, &graph_id)?;
    // Per-observer Song (fix #3): each witness sings into its OWN store + graph, so
    // co-tenants cannot wipe each other's Song. Empty/absent = the shared singleton.
    let observer =
        mcp_arg_string(arguments, &["observer_agent_id", "observerAgentId"]).unwrap_or_default();
    let mut store = read_song_store_for(&graph_dir, &graph_id, &observer)?;
    let verse = mcp_arg_string(arguments, &["verse"]).unwrap_or_default();
    let mode = mcp_arg_string(arguments, &["mode"])
        .unwrap_or_else(|| "verse".to_string())
        .to_ascii_lowercase();
    if !matches!(mode.as_str(), "verse" | "counterpoint" | "coda") {
        return Err(format!(
            "mode must be 'verse', 'counterpoint', or 'coda' (got '{mode}')"
        ));
    }
    let now = timestamp();
    let mut result = serde_json::Map::new();

    match mode.as_str() {
        "verse" => {
            store.verses.insert(
                0,
                LocalSongVerse {
                    text: verse,
                    counterpoints: Vec::new(),
                    created_at: now.clone(),
                    updated_at: now.clone(),
                },
            );
            let mut ejected_count = 0usize;
            if store.verses.len() > MAX_SONG_VERSES {
                let ejected = store.verses.split_off(MAX_SONG_VERSES);
                ejected_count = ejected.len();
                store.archives.push(LocalSongArchiveRecord {
                    archived_at: now.clone(),
                    verses: ejected,
                });
                if let Some(coda) = &mut store.coda {
                    coda.ejections_remaining -= ejected_count as i64;
                    if coda.ejections_remaining <= 0 {
                        store.coda = None;
                    }
                }
            }
            result.insert(
                "verse_count".to_string(),
                serde_json::json!(store.verses.len()),
            );
            result.insert(
                "verseCount".to_string(),
                serde_json::json!(store.verses.len()),
            );
            if ejected_count > 0 {
                result.insert("ejected".to_string(), serde_json::json!(ejected_count));
                let archive_doc = past_songs_doc_id(&observer);
                result.insert("archive_doc_id".to_string(), serde_json::json!(archive_doc));
                result.insert("archiveDocId".to_string(), serde_json::json!(archive_doc));
            }
        }
        "counterpoint" => {
            let verse_index = mcp_song_verse_index(arguments).ok_or_else(|| {
                "verse_index is required for counterpoint mode (0, -1, or -2)".to_string()
            })?;
            let index = if verse_index == 0 {
                0usize
            } else if verse_index < 0 {
                verse_index.unsigned_abs() as usize
            } else {
                return Err(format!(
                    "verse_index must be 0, -1, or -2 (got {verse_index})"
                ));
            };
            if index >= store.verses.len() {
                return Err(format!(
                    "Verse {verse_index} does not exist (Song has {} verses)",
                    store.verses.len()
                ));
            }
            let target = &mut store.verses[index];
            let total_voices = 1 + target.counterpoints.len();
            if total_voices >= MAX_SONG_VOICES {
                return Err(format!(
                    "Verse {verse_index} already has {total_voices} parts (max {MAX_SONG_VOICES}). Cannot add another counterpoint."
                ));
            }
            target.counterpoints.push(verse);
            target.updated_at = now.clone();
            let new_total = 1 + target.counterpoints.len();
            result.insert("verse_index".to_string(), serde_json::json!(verse_index));
            result.insert("verseIndex".to_string(), serde_json::json!(verse_index));
            result.insert("voice_number".to_string(), serde_json::json!(new_total));
            result.insert("voiceNumber".to_string(), serde_json::json!(new_total));
            result.insert("total_voices".to_string(), serde_json::json!(new_total));
            result.insert("totalVoices".to_string(), serde_json::json!(new_total));
        }
        "coda" => {
            store.coda = Some(LocalSongCoda {
                text: verse,
                ejections_remaining: CODA_EJECTION_LIFETIME,
                created_at: now.clone(),
            });
            result.insert("coda_set".to_string(), serde_json::json!(true));
            result.insert("codaSet".to_string(), serde_json::json!(true));
            result.insert(
                "ejections_remaining".to_string(),
                serde_json::json!(CODA_EJECTION_LIFETIME),
            );
            result.insert(
                "ejectionsRemaining".to_string(),
                serde_json::json!(CODA_EJECTION_LIFETIME),
            );
        }
        _ => unreachable!(),
    }

    persist_song_store(&graph_dir, &store)?;
    touch_graph_updated_at(&graph_dir)?;
    touch_profile_updated_at(&app)?;

    if let Some(coda) = &store.coda {
        if !result.contains_key("coda_set") {
            result.insert(
                "coda_ejections_remaining".to_string(),
                serde_json::json!(coda.ejections_remaining),
            );
            result.insert(
                "codaEjectionsRemaining".to_string(),
                serde_json::json!(coda.ejections_remaining),
            );
        }
    }
    result.insert("mode".to_string(), serde_json::json!(mode));
    result.insert("source".to_string(), serde_json::json!("local-song-store"));
    Ok(serde_json::Value::Object(result))
}
