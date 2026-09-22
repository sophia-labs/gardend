use crate::app_runtime::AppHandle;
use crate::{
    loopback_client_token_service::{create_client_token, list_client_tokens, revoke_client_token},
    loopback_client_token_types::{
        CreateLoopbackClientTokenInput, CreateLoopbackClientTokenResponse,
        LoopbackClientTokenSummary, RevokeLoopbackClientTokenInput,
    },
};

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn list_loopback_client_tokens(
    app: AppHandle,
) -> Result<Vec<LoopbackClientTokenSummary>, String> {
    list_client_tokens(&app)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn create_loopback_client_token(
    app: AppHandle,
    input: CreateLoopbackClientTokenInput,
) -> Result<CreateLoopbackClientTokenResponse, String> {
    create_client_token(&app, input)
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn revoke_loopback_client_token(
    app: AppHandle,
    input: RevokeLoopbackClientTokenInput,
) -> Result<LoopbackClientTokenSummary, String> {
    revoke_client_token(&app, input)
}
