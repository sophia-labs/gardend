use crate::app_runtime::AppHandle;
use crate::clock::timestamp;
use axum::{
    extract::{Query, State},
    response::Html,
    routing::get,
    Router,
};
use serde::{Deserialize, Serialize};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener},
    sync::Mutex,
    time::Duration,
};
#[cfg(feature = "desktop")]
use tauri::Emitter;
use tokio::sync::oneshot;

/// Pre-registered Cognito callback URIs MUST match this list. Adding a new
/// port here without registering it on the SPA client (`46raltmjse1gjkkt6hvq30tsk7`)
/// will cause Cognito to reject the redirect with `redirect_mismatch`.
const HOSTED_OAUTH_PORT_RANGE: [u16; 5] = [53682, 53683, 53684, 53685, 53686];
const HOSTED_OAUTH_TIMEOUT: Duration = Duration::from_secs(300);
const HOSTED_OAUTH_CALLBACK_PATH: &str = "/auth/callback";
const HOSTED_OAUTH_CALLBACK_EVENT: &str = "hosted-oauth-callback";

static HOSTED_OAUTH_STATE: Mutex<Option<HostedOAuthHandle>> = Mutex::new(None);

struct HostedOAuthHandle {
    shutdown: oneshot::Sender<()>,
}

#[derive(Debug, Deserialize)]
struct CallbackParams {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct HostedOAuthCallbackEvent {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
    received_at: String,
}

#[derive(Clone)]
struct HandlerState {
    app: AppHandle,
}

#[derive(Debug, Serialize)]
pub(crate) struct StartHostedOauthListenerResponse {
    pub port: u16,
    pub callback_url: String,
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn start_hosted_oauth_listener(
    app: AppHandle,
) -> Result<StartHostedOauthListenerResponse, String> {
    {
        let guard = HOSTED_OAUTH_STATE
            .lock()
            .map_err(|_| "hosted-oauth state lock poisoned".to_string())?;
        if guard.is_some() {
            return Err("hosted oauth listener already running; call stop first".to_string());
        }
    }
    let listener = bind_first_available()?;
    let port = listener
        .local_addr()
        .map_err(|error| format!("read listener addr: {error}"))?
        .port();
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("configure listener: {error}"))?;

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    {
        let mut guard = HOSTED_OAUTH_STATE
            .lock()
            .map_err(|_| "hosted-oauth state lock poisoned".to_string())?;
        *guard = Some(HostedOAuthHandle {
            shutdown: shutdown_tx,
        });
    }

    let handler_state = HandlerState { app: app.clone() };
    let router = Router::new()
        .route(HOSTED_OAUTH_CALLBACK_PATH, get(handle_callback))
        .with_state(handler_state);

    crate::app_runtime::async_runtime::spawn(async move {
        let listener = match tokio::net::TcpListener::from_std(listener) {
            Ok(listener) => listener,
            Err(error) => {
                log::error!("hosted oauth listener init failed: {error}");
                clear_state();
                return;
            }
        };
        let serve_future = axum::serve(listener, router).with_graceful_shutdown(async move {
            tokio::select! {
                _ = shutdown_rx => {
                    log::debug!("hosted oauth listener: shutdown signaled");
                }
                _ = tokio::time::sleep(HOSTED_OAUTH_TIMEOUT) => {
                    log::warn!("hosted oauth listener: timed out without callback");
                }
            }
        });
        if let Err(error) = serve_future.await {
            log::error!("hosted oauth listener stopped: {error}");
        }
        clear_state();
    });

    let callback_url = format!("http://localhost:{port}{HOSTED_OAUTH_CALLBACK_PATH}");
    log::info!("hosted oauth listener armed at {callback_url}");
    Ok(StartHostedOauthListenerResponse { port, callback_url })
}

#[cfg_attr(feature = "desktop", tauri::command)]
pub(crate) fn stop_hosted_oauth_listener() -> Result<(), String> {
    let mut guard = HOSTED_OAUTH_STATE
        .lock()
        .map_err(|_| "hosted-oauth state lock poisoned".to_string())?;
    if let Some(handle) = guard.take() {
        let _ = handle.shutdown.send(());
    }
    Ok(())
}

async fn handle_callback(
    State(state): State<HandlerState>,
    Query(params): Query<CallbackParams>,
) -> Html<String> {
    let success = params.error.is_none() && params.code.is_some() && params.state.is_some();
    let event = HostedOAuthCallbackEvent {
        code: params.code,
        state: params.state,
        error: params.error.clone(),
        error_description: params.error_description.clone(),
        received_at: timestamp(),
    };
    if let Err(error) = state.app.emit(HOSTED_OAUTH_CALLBACK_EVENT, &event) {
        log::warn!("emit hosted-oauth-callback failed: {error}");
    }
    // Best-effort signal shutdown so the listener cleans up promptly.
    if let Ok(mut guard) = HOSTED_OAUTH_STATE.lock() {
        if let Some(handle) = guard.take() {
            let _ = handle.shutdown.send(());
        }
    }
    Html(callback_response_html(
        success,
        params.error_description.as_deref(),
    ))
}

fn bind_first_available() -> Result<TcpListener, String> {
    let mut errors = Vec::with_capacity(HOSTED_OAUTH_PORT_RANGE.len());
    for port in HOSTED_OAUTH_PORT_RANGE {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
        match TcpListener::bind(addr) {
            Ok(listener) => return Ok(listener),
            Err(error) => errors.push(format!("port {port}: {error}")),
        }
    }
    Err(format!(
        "all hosted-oauth ports unavailable: {}",
        errors.join("; ")
    ))
}

fn clear_state() {
    if let Ok(mut guard) = HOSTED_OAUTH_STATE.lock() {
        *guard = None;
    }
}

fn callback_response_html(success: bool, error_description: Option<&str>) -> String {
    let title = if success {
        "Signed in to Sophia"
    } else {
        "Sign-in failed"
    };
    let heading = if success {
        "You're signed in"
    } else {
        "Sign-in failed"
    };
    let body_text = if success {
        "You can close this window and return to Sophia.".to_string()
    } else {
        match error_description {
            Some(detail) if !detail.is_empty() => format!(
                "Sophia couldn't complete the sign-in. {}",
                html_escape(detail)
            ),
            _ => "Sophia couldn't complete the sign-in. Try again from Settings.".to_string(),
        }
    };
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>{title}</title>
<style>
  body {{
    margin: 0;
    font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', system-ui, sans-serif;
    background: #0c0e12;
    color: #eaecef;
    display: flex; align-items: center; justify-content: center;
    min-height: 100vh; padding: 2rem;
  }}
  .card {{
    max-width: 420px; padding: 2.5rem 2rem; border-radius: 12px;
    background: #161a22; box-shadow: 0 8px 24px rgba(0,0,0,0.45);
    text-align: center;
  }}
  h1 {{ margin: 0 0 0.75rem; font-size: 1.4rem; font-weight: 600; }}
  p {{ margin: 0; line-height: 1.5; opacity: 0.85; font-size: 0.95rem; }}
</style>
</head>
<body>
<main class="card">
  <h1>{heading}</h1>
  <p>{body_text}</p>
</main>
<script>
  setTimeout(function () {{ window.close(); }}, 1500);
</script>
</body>
</html>
"#
    )
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_range_matches_documented_set() {
        // Adding a port here without registering the matching Cognito
        // callback URL causes redirect_mismatch failures. Keep this list
        // and the AWS-side allowlist in sync.
        assert_eq!(
            HOSTED_OAUTH_PORT_RANGE,
            [53682u16, 53683, 53684, 53685, 53686]
        );
    }

    #[test]
    fn bind_first_available_returns_a_listener_when_at_least_one_port_is_free() {
        // We can't guarantee a specific port is free in CI, but the function
        // should succeed if any port in the range is available, or surface a
        // clear error otherwise. Run-time correctness is exercised in manual
        // smoke; this test just exercises the happy path on a developer box.
        let result = bind_first_available();
        match result {
            Ok(listener) => {
                let port = listener.local_addr().unwrap().port();
                assert!(HOSTED_OAUTH_PORT_RANGE.contains(&port));
            }
            Err(error) => {
                assert!(error.contains("all hosted-oauth ports unavailable"));
            }
        }
    }

    #[test]
    fn callback_html_success_path_includes_close_script() {
        let html = callback_response_html(true, None);
        assert!(html.contains("You're signed in"));
        assert!(html.contains("window.close()"));
    }

    #[test]
    fn callback_html_failure_path_includes_safe_escaping() {
        let html = callback_response_html(false, Some("<script>alert(1)</script>"));
        assert!(html.contains("Sign-in failed"));
        assert!(!html.contains("<script>alert(1)</script>"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn html_escape_handles_common_special_chars() {
        assert_eq!(
            html_escape(r#"<a href="x">'&"#),
            "&lt;a href=&quot;x&quot;&gt;&#x27;&amp;"
        );
    }
}
