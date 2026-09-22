use crate::app_runtime::AppHandle;
use crate::{
    paths::profile_dir,
    storage::{create_dir_all, display_path},
};
use oxigraph::store::Store;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
};

static PROFILE_METADATA_STORES: OnceLock<Mutex<BTreeMap<PathBuf, Arc<Store>>>> = OnceLock::new();

pub(crate) fn open_profile_metadata_store(app: &AppHandle) -> Result<Arc<Store>, String> {
    let profile_dir = profile_dir(app)?;
    let store_path = profile_dir.join("metadata.oxigraph");

    // Cache hits cannot change the set of physical store incarnations. Avoid
    // stalling every metadata request behind a long durable snapshot walk.
    if let Some(stores) = PROFILE_METADATA_STORES.get() {
        let stores = stores
            .lock()
            .map_err(|_| "profile metadata Oxigraph store cache lock poisoned".to_string())?;
        if store_path.is_dir() {
            if let Some(store) = stores.get(&store_path) {
                return Ok(Arc::clone(store));
            }
        }
    }

    let _lifecycle_guard = crate::rdf_store_service::rdf_store_lifecycle_read_guard()?;
    create_dir_all(&profile_dir)?;
    let mut stores = PROFILE_METADATA_STORES
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .map_err(|_| "profile metadata Oxigraph store cache lock poisoned".to_string())?;

    if store_path.is_dir() {
        if let Some(store) = stores.get(&store_path) {
            return Ok(Arc::clone(store));
        }
    } else {
        stores.remove(&store_path);
    }

    let store = Arc::new(Store::open(&store_path).map_err(|error| {
        format!(
            "open profile metadata Oxigraph store {}: {error}",
            display_path(&store_path)
        )
    })?);
    stores.insert(store_path, Arc::clone(&store));
    Ok(store)
}

/// Snapshot the currently-open profile-level metadata stores as
/// `(store_path, store)` pairs. The durable-plane flusher backs these up via a
/// RocksDB checkpoint instead of plain-copying their files while live.
#[cfg_attr(feature = "desktop", allow(dead_code))]
pub(crate) fn open_profile_metadata_stores() -> Vec<(PathBuf, Arc<Store>)> {
    let Some(stores) = PROFILE_METADATA_STORES.get() else {
        return Vec::new();
    };
    match stores.lock() {
        Ok(stores) => stores
            .iter()
            .map(|(path, store)| (path.clone(), Arc::clone(store)))
            .collect(),
        Err(_) => Vec::new(),
    }
}
