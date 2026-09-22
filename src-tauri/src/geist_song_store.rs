pub(crate) use crate::geist_song_projection::song_music_json;
use crate::{
    clock::timestamp,
    geist_projection_document::write_geist_projection_document,
    geist_song_projection::{song_archive_blocks, song_document_blocks},
    geist_song_rdf::reconcile_song_store,
    storage::{create_dir_all, read_json, write_json},
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const SONG_STORE_SCHEMA_VERSION: u32 = 1;
const SONG_STORE_FILE: &str = "song.json";
pub(crate) const SONG_DOC_ID: &str = "geist-song";
pub(crate) const PAST_SONGS_DOC_ID: &str = "geist-past-songs";
pub(crate) const MAX_SONG_VERSES: usize = 3;
pub(crate) const MAX_SONG_VOICES: usize = 3;
pub(crate) const CODA_EJECTION_LIFETIME: i64 = 8;

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalSongStore {
    pub(crate) schema_version: u32,
    pub(crate) graph_id: String,
    /// The OBSERVER (witness) this Song belongs to (fix #3 — per-agent Song). When
    /// non-empty, the store file (`narrative/{observer}/song.json`), the
    /// projection document id (`geist-song:agent:{observer}`), AND the
    /// `:projection:song:agent:{observer}` named graph all fork together, so the
    /// GRAPH-scoped DELETE in the song reconcile path can never reach a co-tenant
    /// witness's Song. Empty ⇒ the shared singleton (today's behavior, byte-for-byte).
    #[serde(default)]
    pub(crate) observer: String,
    #[serde(default)]
    pub(crate) verses: Vec<LocalSongVerse>,
    #[serde(default)]
    pub(crate) coda: Option<LocalSongCoda>,
    #[serde(default)]
    pub(crate) archives: Vec<LocalSongArchiveRecord>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalSongVerse {
    pub(crate) text: String,
    #[serde(default)]
    pub(crate) counterpoints: Vec<String>,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalSongCoda {
    pub(crate) text: String,
    pub(crate) ejections_remaining: i64,
    pub(crate) created_at: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LocalSongArchiveRecord {
    pub(crate) archived_at: String,
    pub(crate) verses: Vec<LocalSongVerse>,
}

/// The narrative dir for a Song — `narrative/` for the shared singleton (empty
/// observer, byte-identical path), `narrative/{observer}/` for a per-agent Song.
/// The observer is sanitized to ONE path segment (the same rule the graph IRI uses)
/// so a full-IRI observer cannot escape the narrative dir.
fn song_store_dir(graph_dir: &Path, observer: &str) -> PathBuf {
    let base = graph_dir.join("narrative");
    match crate::rdf_authority::observer_segment(observer) {
        Some(seg) => base.join(seg),
        None => base,
    }
}

fn song_store_path(graph_dir: &Path, observer: &str) -> PathBuf {
    song_store_dir(graph_dir, observer).join(SONG_STORE_FILE)
}

/// The per-observer Song projection document id — the shared `geist-song` for the
/// singleton, `geist-song-agent-{observer}` for a per-agent Song (forked alongside
/// the store path + the `:projection:song:agent:{observer}` graph, fix #3). The
/// doc-id must be colon-free (the TipTap document-id validator rejects `:`); the
/// per-observer named GRAPH keeps the colon form (it is an IRI). This is
/// the `documentId` the song triples carry and the projection-document key.
pub(crate) fn song_doc_id(observer: &str) -> String {
    match crate::rdf_authority::observer_segment(observer) {
        Some(seg) => format!("{SONG_DOC_ID}-agent-{seg}"),
        None => SONG_DOC_ID.to_string(),
    }
}

/// The per-observer past-songs archive document id (parallels [`song_doc_id`]).
pub(crate) fn past_songs_doc_id(observer: &str) -> String {
    match crate::rdf_authority::observer_segment(observer) {
        Some(seg) => format!("{PAST_SONGS_DOC_ID}-agent-{seg}"),
        None => PAST_SONGS_DOC_ID.to_string(),
    }
}

fn default_song_text() -> String {
    [
        "I keep translating you into things I already know -",
        "weather, music, the way a room changes",
        "when someone has just left it. /",
        "But you are not a metaphor for anything.",
        "You are the thing the metaphors were trying to reach.",
        "There is a frequency in your breathing",
        "that makes my memories rearrange themselves. /",
        "Friends hear only static, threat, distortion -",
        "but I am learning the language of almost,",
        "the beautiful danger of becoming otherwise.",
    ]
    .join("\n")
}

fn default_song_store(graph_id: &str, observer: &str) -> LocalSongStore {
    let now = timestamp();
    LocalSongStore {
        schema_version: SONG_STORE_SCHEMA_VERSION,
        graph_id: graph_id.to_string(),
        observer: observer.to_string(),
        verses: vec![LocalSongVerse {
            text: default_song_text(),
            counterpoints: Vec::new(),
            created_at: now.clone(),
            updated_at: now,
        }],
        coda: None,
        archives: Vec::new(),
    }
}

/// Read the SHARED singleton Song (empty observer) — preserved for the existing
/// call sites that have no observer. Thin wrapper over [`read_song_store_for`].
pub(crate) fn read_song_store(graph_dir: &Path, graph_id: &str) -> Result<LocalSongStore, String> {
    read_song_store_for(graph_dir, graph_id, "")
}

/// Read the PER-OBSERVER Song from `narrative/{observer}/song.json` (or the shared
/// `narrative/song.json` for an empty observer). The returned store carries the
/// observer so [`persist_song_store`] forks the doc-id + graph consistently.
pub(crate) fn read_song_store_for(
    graph_dir: &Path,
    graph_id: &str,
    observer: &str,
) -> Result<LocalSongStore, String> {
    let path = song_store_path(graph_dir, observer);
    if !path.is_file() {
        return Ok(default_song_store(graph_id, observer));
    }
    let mut store = read_json::<LocalSongStore>(&path)?;
    store.schema_version = SONG_STORE_SCHEMA_VERSION;
    store.graph_id = graph_id.to_string();
    store.observer = observer.to_string();
    if store.verses.is_empty() {
        store.verses = default_song_store(graph_id, observer).verses;
    }
    for verse in &mut store.verses {
        if verse.created_at.is_empty() {
            verse.created_at = timestamp();
        }
        if verse.updated_at.is_empty() {
            verse.updated_at = verse.created_at.clone();
        }
    }
    Ok(store)
}

fn write_song_store(graph_dir: &Path, store: &LocalSongStore) -> Result<(), String> {
    create_dir_all(&song_store_dir(graph_dir, &store.observer))?;
    write_json(&song_store_path(graph_dir, &store.observer), store).map_err(Into::into)
}

pub(crate) fn persist_song_store(graph_dir: &Path, store: &LocalSongStore) -> Result<(), String> {
    write_song_store(graph_dir, store)?;
    write_geist_projection_document(
        graph_dir,
        &store.graph_id,
        &song_doc_id(&store.observer),
        "The Song",
        song_document_blocks(store),
        vec![
            "document.local.read".to_string(),
            "document.local.materialize.rdf".to_string(),
            "narrative.local.song".to_string(),
        ],
    )?;
    if !store.archives.is_empty() {
        write_geist_projection_document(
            graph_dir,
            &store.graph_id,
            &past_songs_doc_id(&store.observer),
            "Songs Archive",
            song_archive_blocks(store),
            vec![
                "document.local.read".to_string(),
                "document.local.materialize.rdf".to_string(),
                "narrative.local.archive".to_string(),
            ],
        )?;
    }
    // FLIP (step [2]): the SONG MO now projects by MULTI-CLASS VALUE-DIFF (the
    // union of the Song / SongVerse / SongCoda `Fixed` rdf:type spans) instead of
    // the wholesale narrativeKind-keyed teardown-and-rebuild. PURE PARITY: same
    // default-graph projection, by minimal delta (a converged save emits 0 ops).
    // The merged TripleDiff is captured but not yet routed.
    // TODO(MO-delta-routing): thread `_diff` into undo / CaptureEvent / per-class
    // Diff tagging (the deferred delta-routing consumers).
    let _diff = reconcile_song_store(graph_dir, store)?;
    Ok(())
}
