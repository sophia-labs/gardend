use crate::{
    loopback_client_token_types::LoopbackClientTokenStore,
    profile_paths::LOOPBACK_CLIENT_TOKENS_FILE,
    storage::{read_json, write_secret_json},
};
use std::path::Path;

pub(crate) fn read_token_store_in(profile_dir: &Path) -> Result<LoopbackClientTokenStore, String> {
    read_token_store_at(&profile_dir.join(LOOPBACK_CLIENT_TOKENS_FILE))
}

fn read_token_store_at(path: &Path) -> Result<LoopbackClientTokenStore, String> {
    if !path.is_file() {
        return Ok(LoopbackClientTokenStore::default());
    }
    read_json::<LoopbackClientTokenStore>(path).map_err(Into::into)
}

pub(crate) fn write_token_store_in(
    profile_dir: &Path,
    store: &LoopbackClientTokenStore,
) -> Result<(), String> {
    write_secret_json(&profile_dir.join(LOOPBACK_CLIENT_TOKENS_FILE), store).map_err(Into::into)
}
