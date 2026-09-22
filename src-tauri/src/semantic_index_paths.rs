use crate::runtime_config::SEMANTIC_INDEX_FILE;
use std::path::{Path, PathBuf};

pub(crate) fn semantic_index_dir(graph_dir: &Path) -> PathBuf {
    graph_dir.join("indexes/semantic")
}

pub(crate) fn semantic_index_path(graph_dir: &Path) -> PathBuf {
    semantic_index_dir(graph_dir).join(SEMANTIC_INDEX_FILE)
}

pub(crate) fn semantic_scaffold_path(graph_dir: &Path) -> PathBuf {
    semantic_index_dir(graph_dir).join("scaffold.json")
}

pub(crate) fn semantic_relation_profiles_path(graph_dir: &Path) -> PathBuf {
    semantic_index_dir(graph_dir).join("relation-profiles.json")
}

pub(crate) fn semantic_vectors_dir(graph_dir: &Path) -> PathBuf {
    semantic_index_dir(graph_dir).join("vectors")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_index_paths_match_profile_layout() {
        let graph_dir = PathBuf::from("/tmp/mnemosyne-graph");
        assert_eq!(
            semantic_index_path(&graph_dir),
            graph_dir.join("indexes/semantic/blocks.json")
        );
        assert_eq!(
            semantic_scaffold_path(&graph_dir),
            graph_dir.join("indexes/semantic/scaffold.json")
        );
        assert_eq!(
            semantic_relation_profiles_path(&graph_dir),
            graph_dir.join("indexes/semantic/relation-profiles.json")
        );
        assert_eq!(
            semantic_vectors_dir(&graph_dir),
            graph_dir.join("indexes/semantic/vectors")
        );
    }
}
