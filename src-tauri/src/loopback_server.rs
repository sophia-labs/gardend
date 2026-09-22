use crate::app_runtime::AppHandle;
use crate::{
    cell_graph_boundary::CellGraphBoundary,
    clock::timestamp,
    local_jobs::LocalJobRegistry,
    local_service_host::LocalServiceHost,
    loopback_router::loopback_router,
    loopback_state::{LoopbackManifest, LoopbackState},
    loopback_token_grants::session_all_token_grant,
    paths::{jobs_dir, loopback_manifest_path},
    profile_service::ensure_profile,
    runtime_config::{init_self_heal_graphs_from_env, LOOPBACK_BIND_HOST, RUNTIME_PROFILE},
    storage::{display_path, write_secret_json},
};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener},
    process,
    sync::Arc,
};
#[cfg(feature = "desktop")]
use tauri::Manager;
use uuid::Uuid;

pub(super) fn start_loopback_server(app: AppHandle) -> Result<LoopbackManifest, String> {
    // `setup_core` installs this before journal recovery. Reuse that exact
    // owner here so ingress, recovered work, and schedulers cannot disagree.
    let cell_graph = app.state::<Arc<CellGraphBoundary>>().inner().clone();
    ensure_profile(&app)?;
    // Container/headless overrides; defaults preserve desktop behavior
    // (127.0.0.1, OS-assigned port, per-run random token).
    init_self_heal_graphs_from_env();
    let bind_ip: IpAddr = match std::env::var("GARDEN_LOOPBACK_HOST") {
        Ok(host) if !host.trim().is_empty() => host
            .trim()
            .parse()
            .map_err(|error| format!("parse GARDEN_LOOPBACK_HOST: {error}"))?,
        _ => IpAddr::V4(Ipv4Addr::LOCALHOST),
    };
    let bind_port: u16 = match std::env::var("GARDEN_LOOPBACK_PORT") {
        Ok(port) if !port.trim().is_empty() => port
            .trim()
            .parse()
            .map_err(|error| format!("parse GARDEN_LOOPBACK_PORT: {error}"))?,
        _ => 0,
    };
    let listener = TcpListener::bind(SocketAddr::new(bind_ip, bind_port))
        .map_err(|error| format!("bind loopback API: {error}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("configure loopback API socket: {error}"))?;
    let addr = listener
        .local_addr()
        .map_err(|error| format!("read loopback API address: {error}"))?;
    let token = match std::env::var("GARDEN_LOOPBACK_TOKEN") {
        Ok(token) if !token.trim().is_empty() => token.trim().to_string(),
        _ => Uuid::new_v4().simple().to_string(),
    };
    // Behind a gateway/proxy the cell's loopback address is meaningless to
    // clients — advertise the public base (e.g. https://gw/g/{graph}) when
    // the operator provides one.
    let api_url = match std::env::var("GARDEN_PUBLIC_BASE_URL") {
        Ok(base) if !base.trim().is_empty() => base.trim().trim_end_matches('/').to_string(),
        _ => format!("http://{LOOPBACK_BIND_HOST}:{}", addr.port()),
    };
    let mcp_url = format!("{api_url}/mcp");
    let openapi_url = format!("{api_url}/openapi.json");
    let manifest_path = loopback_manifest_path(&app)?;
    let jobs = Arc::new(LocalJobRegistry::new_bound(jobs_dir(&app)?, &cell_graph)?);
    let lifecycle = app.state::<Arc<crate::cell_lifecycle::CellLifecycle>>();
    let lifecycle = lifecycle.inner().clone();
    let token_grant = session_all_token_grant();
    let manifest = LoopbackManifest {
        runtime_profile: RUNTIME_PROFILE,
        bind_host: LOOPBACK_BIND_HOST,
        port: addr.port(),
        api_url,
        mcp_url,
        openapi_url,
        token: token.clone(),
        pid: process::id(),
        started_at: timestamp(),
        manifest_path: display_path(&manifest_path),
        auth_header: "Authorization: Bearer <token>",
        token_audience: "tauri-runtime-compatibility",
        token_storage: "plaintext-owner-only-profile-manifest",
        security_warning: "The per-run session token currently grants every local loopback/MCP scope for compatibility. Do not share loopback.json or expose this token to third-party clients.",
        cell_graph_id: cell_graph.owner_graph_id().map(str::to_string),
        cell_owner: cell_graph.owner_principal().map(str::to_string),
        cell_generation: cell_graph.graph_generation(),
        cell_registry_revision: cell_graph.registry_revision(),
        capabilities: token_grant.scopes.clone(),
        token_scope_mode: token_grant.scope_mode,
        token_scopes: token_grant.scopes,
        scope_details: token_grant.scope_details,
        grant_profiles: token_grant.grant_profiles,
    };
    write_secret_json(&manifest_path, &manifest)?;

    let app_for_state = app.clone();
    let state = Arc::new(LoopbackState {
        app,
        token,
        manifest: manifest.clone(),
        jobs,
        services: Arc::new(LocalServiceHost::default()),
        lifecycle,
        cell_graph,
    });
    app_for_state.manage(state.clone());
    let router = loopback_router(state);
    crate::app_runtime::async_runtime::spawn(async move {
        let listener = match tokio::net::TcpListener::from_std(listener) {
            Ok(listener) => listener,
            Err(error) => {
                log::error!("Failed to create loopback API listener: {error}");
                return;
            }
        };
        if let Err(error) = axum::serve(listener, router).await {
            log::error!("loopback API stopped: {error}");
        }
    });

    log::info!(
        "Loopback API listening at {}; manifest {}",
        manifest.port,
        manifest.manifest_path
    );
    Ok(manifest)
}
