use crate::app_runtime::AppHandle;
use crate::{
    loopback_client_token_types::LoopbackClientTokenStore,
    paths::loopback_client_tokens_path,
    storage::{read_json, write_secret_json},
};

pub(crate) fn read_token_store(app: &AppHandle) -> Result<LoopbackClientTokenStore, String> {
    let path = loopback_client_tokens_path(app)?;
    if !path.is_file() {
        return Ok(LoopbackClientTokenStore::default());
    }
    read_json::<LoopbackClientTokenStore>(&path).map_err(Into::into)
}

pub(crate) fn write_token_store(
    app: &AppHandle,
    store: &LoopbackClientTokenStore,
) -> Result<(), String> {
    write_secret_json(&loopback_client_tokens_path(app)?, store).map_err(Into::into)
}
