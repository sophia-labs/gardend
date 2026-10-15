//! Admission for the v1 all-in-one offline bundle, not a graph quota.
//! The current wire format retains histories, originals and multiple RDF/JSON
//! copies simultaneously. Larger graphs remain online; they need a future
//! bounded transfer protocol rather than an incomplete `complete: true` bundle.
use crate::app_error::{AppError, AppResult};
use std::{fs, path::Path};

pub(crate) const MAX_SOURCE_PULL_BYTES: u64 = 32 * 1024 * 1024;
const MAX_ENTRIES: usize = 20_000;
const MAX_DEPTH: usize = 32;
/// Walks taken again when an entry the walk listed is gone once inspected.
const VANISHED_RETRIES: usize = 4;

pub(crate) fn capacity_error() -> AppError {
    AppError::capacity("Complete offline source mirror exceeds this transfer's capacity. Online graph access and retained source data are unchanged; no partial mirror was returned.")
        .with_code(crate::app_error_codes::SOURCE_BUNDLE_TOO_LARGE)
}

/// Metadata only, before source initialization/checkpoint/repair. Count every
/// graph file conservatively, including immutable histories and the RDF store.
/// Do not follow links or treat unreadable/vanished files as empty.
pub(crate) fn check_graph(root: &Path) -> AppResult<()> {
    check_graph_with_limit(root, MAX_SOURCE_PULL_BYTES)
}

/// Enforce the accumulated byte limit on the actual reads as well as metadata:
/// a file growing after preflight must not turn into an unbounded allocation.
pub(crate) fn read_bytes(path: &Path, used: &mut u64) -> AppResult<Vec<u8>> {
    use std::io::Read;
    let remaining = MAX_SOURCE_PULL_BYTES.checked_sub(*used).ok_or_else(capacity_error)?;
    let file = fs::File::open(path).map_err(|_| AppError::storage("Cannot read offline source input"))?;
    let metadata = file.metadata().map_err(|_| AppError::storage("Cannot inspect open offline source input"))?;
    if !metadata.is_file() { return Err(AppError::storage("Offline source input is not a regular file")); }
    if metadata.len() > remaining { return Err(capacity_error()); }
    let mut bytes = Vec::new();
    file.take(remaining + 1).read_to_end(&mut bytes).map_err(|_| AppError::storage("Cannot read offline source input"))?;
    if bytes.len() as u64 > remaining { return Err(capacity_error()); }
    *used += bytes.len() as u64;
    Ok(bytes)
}

fn check_graph_with_limit(root: &Path, limit: u64) -> AppResult<()> {
    // A live graph churns under the walk: RocksDB deletes the SST files a
    // background compaction has merged (right after a write burst, when a
    // follower pulls), and atomic writers rename their temp files into place.
    // An entry listed a moment ago and gone once inspected is that churn, not
    // an unreadable input: walk again, never counting it as empty. Churn that
    // does not settle, and every other failure, still refuses.
    let mut attempt = 0;
    loop {
        match walk(root, limit) {
            Ok(()) => return Ok(()),
            Err(Walk::Vanished) if attempt < VANISHED_RETRIES => attempt += 1,
            Err(Walk::Vanished) => {
                return Err(AppError::storage("Cannot inspect offline source transfer input"))
            }
            Err(Walk::Refused(error)) => return Err(error),
        }
    }
}

enum Walk {
    Vanished,
    Refused(AppError),
}

fn walk(root: &Path, limit: u64) -> Result<(), Walk> {
    fn vanished_or(error: std::io::Error, message: &'static str) -> Walk {
        if error.kind() == std::io::ErrorKind::NotFound {
            Walk::Vanished
        } else {
            Walk::Refused(AppError::storage(message))
        }
    }
    fn visit(path: &Path, depth: usize, bytes: &mut u64, entries: &mut usize, limit: u64) -> Result<(), Walk> {
        *entries += 1;
        if depth > MAX_DEPTH || *entries > MAX_ENTRIES { return Err(Walk::Refused(capacity_error())); }
        let metadata = fs::symlink_metadata(path).map_err(|error| vanished_or(error, "Cannot inspect offline source transfer input"))?;
        if metadata.file_type().is_symlink() { return Err(Walk::Refused(AppError::storage("Offline source input contains a symbolic link"))); }
        if metadata.is_dir() {
            for entry in fs::read_dir(path).map_err(|error| vanished_or(error, "Cannot enumerate offline source transfer input"))? {
                let entry = entry.map_err(|error| vanished_or(error, "Cannot inspect offline source transfer entry"))?;
                visit(&entry.path(), depth + 1, bytes, entries, limit)?;
            }
        } else if metadata.is_file() {
            *bytes = bytes.checked_add(metadata.len()).ok_or_else(|| Walk::Refused(capacity_error()))?;
            if *bytes > limit { return Err(Walk::Refused(capacity_error())); }
        } else { return Err(Walk::Refused(AppError::storage("Offline source input is not a regular file"))); }
        Ok(())
    }
    visit(root, 0, &mut 0, &mut 0, limit)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(std::path::PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("source-pull-budget-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&path).unwrap(); Self(path)
        }
        fn path(&self) -> &Path { &self.0 }
    }
    impl Drop for Fixture { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); } }
    #[test]
    fn source_pull_budget_checks_accumulated_and_single_file_without_reading_payloads() {
        let root = Fixture::new();
        fs::write(root.path().join("a"), b"12345").unwrap();
        fs::write(root.path().join("b"), b"12345").unwrap();
        assert!(check_graph_with_limit(root.path(), 10).is_ok());
        assert!(check_graph_with_limit(root.path(), 9).is_err());
        let file = fs::File::create(root.path().join("history.json")).unwrap();
        file.set_len(2_914_598_700).unwrap(); // sparse, actual incident scale; never deserialized
        let error = check_graph(root.path()).unwrap_err();
        assert_eq!(error.code(), Some(crate::app_error_codes::SOURCE_BUNDLE_TOO_LARGE));
        assert_eq!(file.metadata().unwrap().len(), 2_914_598_700);
        assert!(!root.path().join("source-sync").exists());
    }
    #[cfg(unix)]
    #[test]
    fn source_pull_budget_refuses_links_and_observes_growth_on_recheck() {
        let root = Fixture::new();
        fs::write(root.path().join("a"), b"12345").unwrap();
        assert!(check_graph_with_limit(root.path(), 5).is_ok());
        fs::write(root.path().join("a"), b"123456").unwrap();
        assert!(check_graph_with_limit(root.path(), 5).is_err());
        std::os::unix::fs::symlink(root.path().join("a"), root.path().join("link")).unwrap();
        assert!(check_graph(root.path()).is_err());
    }
    /// A real Oxigraph (RocksDB) store, written, flushed and compacted on
    /// another thread while the budget walks its directory, as a follower's pull
    /// walks a graph right after the write it heard. Compaction deletes the SST
    /// files it merged; the walk saw some of them vanish between listing and
    /// inspecting and refused the pull ("Cannot inspect offline source transfer
    /// input"). Bounded: five seconds of walks.
    #[test]
    fn source_pull_budget_walks_a_store_while_rocksdb_compacts_it() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let root = Fixture::new();
        let store = Arc::new(oxigraph::store::Store::open(root.path().join("store.oxigraph")).unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let (store, stop) = (store.clone(), stop.clone());
            std::thread::spawn(move || {
                let predicate = oxigraph::model::NamedNode::new_unchecked("urn:budget:p");
                let mut round = 0u64;
                while !stop.load(Ordering::SeqCst) {
                    round += 1;
                    let quads = (0..200)
                        .map(|index| {
                            oxigraph::model::Quad::new(
                                oxigraph::model::NamedNode::new_unchecked(format!("urn:budget:s{round}-{index}")),
                                predicate.clone(),
                                oxigraph::model::Literal::new_simple_literal("x".repeat(64)),
                                oxigraph::model::GraphName::DefaultGraph,
                            )
                        })
                        .collect::<Vec<_>>();
                    store.extend(quads.clone()).unwrap();
                    store.flush().unwrap();
                    for quad in &quads {
                        store.remove(quad).unwrap();
                    }
                    store.flush().unwrap();
                    store.optimize().unwrap();
                }
                round
            })
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let (mut walks, mut refused) = (0usize, Vec::new());
        while std::time::Instant::now() < deadline {
            walks += 1;
            if let Err(error) = check_graph(root.path()) {
                refused.push(error.to_string());
            }
        }
        stop.store(true, Ordering::SeqCst);
        let rounds = writer.join().unwrap();
        assert!(rounds > 0, "the writer compacted at least once");
        assert!(
            refused.is_empty(),
            "{} of {walks} walks refused a store that was only compacting ({rounds} rounds): {:?}",
            refused.len(),
            refused.iter().take(3).collect::<Vec<_>>()
        );
    }
    #[test]
    fn source_pull_budget_reads_enforce_accumulated_bytes_after_metadata() {
        let root = Fixture::new();
        fs::write(root.path().join("a"), b"123456").unwrap();
        let mut used = MAX_SOURCE_PULL_BYTES - 6;
        assert_eq!(read_bytes(&root.path().join("a"), &mut used).unwrap(), b"123456");
        assert!(read_bytes(&root.path().join("a"), &mut used).is_err());
        assert_eq!(used, MAX_SOURCE_PULL_BYTES);
    }
}
