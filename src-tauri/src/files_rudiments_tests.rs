//! Files rudiments (round 2026-10-05): the cell side of
//! the Files contract v1 (CONTRACT-files, kept outside this repository).
//!
//! Every case drives the REAL axum router (`loopback_router`) through the
//! single-graph cell boundary with a signed `x-sophia-cell-lease`, the same
//! technique as `owned_restore_tests.rs`, so route classification in the
//! closed cell policy is exercised together with the handlers. No mocks: the
//! bytes go to a disposable profile on disk, through the real CRDT queue,
//! workspace Y.Doc, RDF projection and (for the restart case) the real
//! durable flush and hydrate.
//!
//! Written red first, against `987ac57` plus nothing: every route below is
//! absent there, the `garden-file-views` pack is unknown, and the capability
//! has no `files` block.
use super::*;
use axum::{body::Body, http::Request};
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine,
};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use tower::ServiceExt;

const GRAPH: &str = "files-rudiments-fixture-20261005";
const OWNER: &str = "user:files-rudiments-owner-20261005";
const SECRET: &[u8] = b"files-rudiments-test-secret-at-least-32-bytes";
const TOKEN: &str = "files-rudiments-disposable-token";
const BOUNDARY: &str = "files-rudiments-boundary-7d3a";
const CAP: usize = 50 * 1024 * 1024;

fn lease(role: &str) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let claims = json!({"iss":"pn-gateway","aud":"gardend-cell","sub":OWNER,"owner":OWNER,
        "graphId":GRAPH,"generation":1,"cellId":bound_cell_id(OWNER,GRAPH,1),"role":role,
        "policyRevision":1,"registryRevision":1,"sessionId":"files-rudiments-test","iat":now,"exp":now+600});
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
    let signed = format!("{header}.{payload}");
    let mut mac = Hmac::<Sha256>::new_from_slice(SECRET).unwrap();
    mac.update(signed.as_bytes());
    format!(
        "{signed}.{}",
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    )
}

fn state_for(profile: &std::path::Path) -> Arc<LoopbackState> {
    let app = crate::tauri_runtime::build_mock_cell_app_for_tests(GRAPH, OWNER, 1);
    let boundary = Arc::new(
        CellGraphBoundary::new_with_binding(
            Some(GRAPH.into()),
            Some(OWNER.into()),
            Some(1),
            Some(1),
            Some(SECRET.to_vec()),
        )
        .unwrap(),
    );
    let jobs = Arc::new(
        crate::local_jobs::LocalJobRegistry::new_bound(profile.join("jobs"), &boundary).unwrap(),
    );
    let grant = crate::loopback_token_grants::session_all_token_grant();
    let manifest = crate::loopback_state::LoopbackManifest {
        runtime_profile: "test",
        bind_host: "127.0.0.1",
        port: 0,
        api_url: "test".into(),
        mcp_url: "test".into(),
        openapi_url: "test".into(),
        token: TOKEN.into(),
        pid: 0,
        started_at: "test".into(),
        manifest_path: "test".into(),
        auth_header: "test",
        token_audience: "test",
        token_storage: "test",
        security_warning: "test",
        cell_graph_id: Some(GRAPH.into()),
        cell_owner: Some(OWNER.into()),
        cell_generation: Some(1),
        cell_registry_revision: Some(1),
        capabilities: grant.scopes.clone(),
        token_scope_mode: grant.scope_mode,
        token_scopes: grant.scopes,
        scope_details: grant.scope_details,
        grant_profiles: grant.grant_profiles,
    };
    Arc::new(LoopbackState {
        app,
        token: TOKEN.into(),
        manifest,
        jobs,
        services: Arc::new(crate::local_service_host::LocalServiceHost::default()),
        lifecycle: Arc::new(crate::cell_lifecycle::CellLifecycle::new()),
        cell_graph: boundary,
    })
}

struct Reply {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: Vec<u8>,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|error| {
            panic!(
                "status {} body is not JSON ({error}): {}",
                self.status,
                String::from_utf8_lossy(&self.body[..self.body.len().min(512)])
            )
        })
    }
    fn header(&self, name: &str) -> String {
        self.headers
            .get(name)
            .map(|value| value.to_str().unwrap().to_string())
            .unwrap_or_default()
    }
}

async fn call(
    state: &Arc<LoopbackState>,
    method: &str,
    path: &str,
    role: &str,
    content_type: Option<&str>,
    body: Vec<u8>,
) -> Reply {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("x-sophia-cell-lease", lease(role));
    if let Some(content_type) = content_type {
        request = request.header("content-type", content_type);
    }
    let response = crate::loopback_router::loopback_router(state.clone())
        .oneshot(request.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = axum::body::to_bytes(response.into_body(), 256 * 1024 * 1024)
        .await
        .unwrap()
        .to_vec();
    Reply {
        status,
        headers,
        body,
    }
}

async fn get(state: &Arc<LoopbackState>, path: &str) -> Reply {
    call(state, "GET", path, "viewer", None, Vec::new()).await
}

async fn send_json(state: &Arc<LoopbackState>, method: &str, path: &str, body: Value) -> Reply {
    call(
        state,
        method,
        path,
        "editor",
        Some("application/json"),
        serde_json::to_vec(&body).unwrap(),
    )
    .await
}

/// (name, filename, content type, bytes)
fn multipart(parts: &[(&str, Option<&str>, Option<&str>, &[u8])]) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, filename, content_type, bytes) in parts {
        body.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        match filename {
            Some(filename) => body.extend_from_slice(
                format!(
                    "Content-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\n"
                )
                .as_bytes(),
            ),
            None => body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"\r\n").as_bytes(),
            ),
        }
        if let Some(content_type) = content_type {
            body.extend_from_slice(format!("Content-Type: {content_type}\r\n").as_bytes());
        }
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    body
}

fn multipart_type() -> String {
    format!("multipart/form-data; boundary={BOUNDARY}")
}

async fn upload(
    state: &Arc<LoopbackState>,
    filename: &str,
    content_type: &str,
    bytes: &[u8],
    extra: &[(&str, &str)],
) -> Reply {
    let mut parts: Vec<(&str, Option<&str>, Option<&str>, &[u8])> = extra
        .iter()
        .map(|(name, value)| (*name, None, None, value.as_bytes()))
        .collect();
    parts.push(("file", Some(filename), Some(content_type), bytes));
    call(
        state,
        "POST",
        &format!("/artifacts/{GRAPH}/files"),
        "editor",
        Some(multipart_type().as_str()),
        multipart(&parts),
    )
    .await
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Every byte value, repeated: a non-text, non-PDF, non-image payload.
fn binary_fixture(len: usize) -> Vec<u8> {
    (0..len).map(|index| (index * 7 % 256) as u8).collect()
}

fn pdf_fixture() -> Vec<u8> {
    b"%PDF-1.4\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n2 0 obj << /Type /Pages /Kids [] /Count 0 >> endobj\ntrailer << /Root 1 0 R >>\n%%EOF\n".to_vec()
}

fn png_fixture() -> Vec<u8> {
    let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    bytes.extend_from_slice(&[0, 0, 0, 13, b'I', b'H', b'D', b'R', 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0]);
    bytes.extend_from_slice(&[0x1F, 0x15, 0xC4, 0x89]);
    bytes
}

fn navigation_artifact(navigation: &Value, artifact_id: &str) -> Option<Value> {
    navigation["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|artifact| artifact["id"] == artifact_id)
        .cloned()
}

/// Run one case in a disposable profile with the graph created.
fn with_profile<F, Fut>(name: &str, case: F)
where
    F: FnOnce(std::path::PathBuf) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let root = std::env::temp_dir().join(format!("garden-{name}-{}", uuid::Uuid::new_v4()));
    let profile = root.join("profile");
    std::fs::create_dir_all(&profile).unwrap();
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let case_root = root.clone();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let app = crate::tauri_runtime::build_mock_cell_app_for_tests(GRAPH, OWNER, 1);
                crate::graph_service::create_graph_service(
                    &app,
                    crate::graph_service::CreateGraphInput {
                        graph_id: Some(GRAPH.into()),
                        title: "Files rudiments fixture".into(),
                        description: None,
                        operation_id: None,
                    },
                )
                .unwrap();
                drop(app);
                case(case_root).await;
            })
    }));
    std::env::remove_var("GARDEN_PROFILE_DIR");
    let _ = std::fs::remove_dir_all(&root);
    if let Err(error) = outcome {
        std::panic::resume_unwind(error);
    }
}

fn profile_of(root: &std::path::Path) -> std::path::PathBuf {
    root.join("profile")
}

#[test]
fn files_rudiments_capability_advertises_files_v1() {
    with_profile("files-capability", |root| async move {
        let state = state_for(&profile_of(&root));
        let reply = get(&state, "/api/capabilities").await;
        assert_eq!(reply.status, StatusCode::OK);
        let files = &reply.json()["files"];
        assert_eq!(files["version"], 1, "{}", reply.json());
        assert_eq!(files["maxUploadBytes"], CAP as u64);
        assert_eq!(files["parsing"], false);
        assert_eq!(files["upload"], "POST /artifacts/{graph_id}/files");
        assert_eq!(files["folderViews"]["vocab"], "garden-file-views");
        assert_eq!(files["folderViews"]["version"], "1.0.0");
    });
}

#[test]
fn files_rudiments_upload_non_pdf_binary_round_trips_exact_bytes() {
    with_profile("files-binary", |root| async move {
        let state = state_for(&profile_of(&root));
        let bytes = binary_fixture(300_001);
        let reply = upload(&state, "archive.bin", "application/octet-stream", &bytes, &[]).await;
        assert_eq!(reply.status, StatusCode::CREATED, "{}", String::from_utf8_lossy(&reply.body));
        let created = reply.json();
        let id = created["id"].as_str().unwrap().to_string();
        assert!(id.starts_with("file-"), "{created}");
        assert_eq!(created["artifactId"], id);
        assert_eq!(created["label"], "archive.bin");
        assert_eq!(created["originalFilename"], "archive.bin");
        assert_eq!(created["mimeType"], "application/octet-stream");
        assert_eq!(created["sizeBytes"].as_f64().unwrap() as usize, bytes.len());
        assert_eq!(created["sha256"], sha256_hex(&bytes));
        assert_eq!(created["status"], "ready");
        assert_eq!(created["fileType"], "bin");
        assert_eq!(created["parentId"], Value::Null);
        assert_eq!(created["replayed"], false);

        let navigation = get(&state, &format!("/navigation/{GRAPH}")).await.json();
        let listed = navigation_artifact(&navigation, &id).expect("upload is listed in navigation");
        assert_eq!(listed["label"], "archive.bin");
        assert_eq!(listed["sizeBytes"].as_f64().unwrap() as usize, bytes.len());

        let download = get(&state, &format!("/artifacts/{GRAPH}/{id}/download")).await;
        assert_eq!(download.status, StatusCode::OK);
        assert!(download.body == bytes, "downloaded bytes differ");
        assert_eq!(download.header("content-type"), "application/octet-stream");
        assert_eq!(download.header("content-length"), bytes.len().to_string());
        assert!(download.header("content-disposition").starts_with("attachment;"));
        assert_eq!(download.header("x-content-type-options"), "nosniff");
        assert_eq!(download.header("content-security-policy"), "sandbox");
        // Inline is refused for a type the browser could execute or sniff.
        let inline = get(&state, &format!("/artifacts/{GRAPH}/{id}/download?inline=1")).await;
        assert!(inline.header("content-disposition").starts_with("attachment;"));

        // The bytes are in the graph: workspace RDF carries the file.
        let graph_dir = crate::paths::existing_graph_dir(&state.app, GRAPH).unwrap();
        assert!(graph_dir.join("artifacts").join(&id).join("original").join("manifest.json").is_file());
        // Nothing is staged after success.
        let pending = graph_dir.join("pending-uploads");
        assert!(!pending.exists() || std::fs::read_dir(&pending).unwrap().next().is_none());
    });
}

#[test]
fn files_rudiments_upload_pdf_and_image_keep_type_and_inline_preview() {
    with_profile("files-pdf-image", |root| async move {
        let state = state_for(&profile_of(&root));
        for (filename, mime, bytes, file_type) in [
            ("Paper.PDF", "application/pdf", pdf_fixture(), "pdf"),
            ("photo.png", "image/png", png_fixture(), "png"),
        ] {
            let reply = upload(&state, filename, mime, &bytes, &[("label", "Shown name")]).await;
            assert_eq!(reply.status, StatusCode::CREATED, "{}", String::from_utf8_lossy(&reply.body));
            let created = reply.json();
            assert_eq!(created["mimeType"], mime);
            assert_eq!(created["fileType"], file_type);
            assert_eq!(created["label"], "Shown name");
            assert_eq!(created["originalFilename"], filename);
            let id = created["id"].as_str().unwrap();
            let inline = get(&state, &format!("/artifacts/{GRAPH}/{id}/download?inline=1")).await;
            assert_eq!(inline.status, StatusCode::OK);
            assert!(inline.body == bytes);
            assert_eq!(inline.header("content-type"), mime);
            assert!(inline.header("content-disposition").starts_with("inline;"), "{filename}");
            let attachment = get(&state, &format!("/artifacts/{GRAPH}/{id}/download")).await;
            assert!(attachment.header("content-disposition").starts_with("attachment;"));
        }
        // SVG is an image the browser executes: never inline.
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg"><script>1</script></svg>"#;
        let reply = upload(&state, "x.svg", "image/svg+xml", svg, &[]).await;
        assert_eq!(reply.status, StatusCode::CREATED);
        let id = reply.json()["id"].as_str().unwrap().to_string();
        let inline = get(&state, &format!("/artifacts/{GRAPH}/{id}/download?inline=1")).await;
        assert!(inline.header("content-disposition").starts_with("attachment;"));
    });
}

#[test]
fn files_rudiments_cap_accepts_exactly_50_mib_and_refuses_one_byte_more() {
    with_profile("files-cap", |root| async move {
        let state = state_for(&profile_of(&root));
        let graph_dir = crate::paths::existing_graph_dir(&state.app, GRAPH).unwrap();
        {
            let exact = vec![0x5Au8; CAP];
            let reply = upload(&state, "exact.dat", "application/octet-stream", &exact, &[("artifactId", "exact-cap")]).await;
            assert_eq!(reply.status, StatusCode::CREATED, "{}", String::from_utf8_lossy(&reply.body));
            assert_eq!(reply.json()["sizeBytes"].as_f64().unwrap() as usize, CAP);
        }
        {
            let over = vec![0x5Au8; CAP + 1];
            let reply = upload(&state, "over.dat", "application/octet-stream", &over, &[("artifactId", "over-cap")]).await;
            assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
            let body = reply.json();
            assert_eq!(body["code"], "file_too_large", "{body}");
            assert_eq!(body["maxBytes"], CAP as u64);
            assert_eq!(body["ok"], false);
        }
        assert_eq!(
            get(&state, &format!("/navigation/{GRAPH}/artifacts/over-cap")).await.status,
            StatusCode::NOT_FOUND
        );
        assert!(!graph_dir.join("artifacts").join("over-cap").exists(), "refused bytes were kept");
        let pending = graph_dir.join("pending-uploads");
        assert!(
            !pending.exists() || std::fs::read_dir(&pending).unwrap().next().is_none(),
            "refused upload left a staged file"
        );

        // The older byte routes enforce the same cap.
        {
            let over = STANDARD.encode(vec![0x11u8; CAP + 1]);
            let reply = send_json(
                &state,
                "POST",
                &format!("/artifacts/{GRAPH}/over-revision/revisions"),
                json!({"dataBase64": over, "mimeType": "application/octet-stream", "filename": "over.dat"}),
            )
            .await;
            assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
            assert_eq!(reply.json()["code"], "file_too_large");
            assert!(!graph_dir.join("artifacts").join("over-revision").exists());
        }
        {
            let over = STANDARD.encode(vec![0x22u8; CAP + 1]);
            let reply = send_json(
                &state,
                "PUT",
                &format!("/navigation/{GRAPH}/artifacts/over-put"),
                json!({"label": "over", "dataBase64": over, "mimeType": "application/octet-stream"}),
            )
            .await;
            assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
            assert_eq!(reply.json()["code"], "file_too_large");
            assert!(!graph_dir.join("artifacts").join("over-put").exists());
        }
        {
            // Cell mode: the parsing upload is capped too (it would otherwise stage 500 MiB).
            let over = vec![0x33u8; CAP + 1];
            let reply = call(
                &state,
                "POST",
                &format!("/artifacts/{GRAPH}/upload"),
                "editor",
                Some(multipart_type().as_str()),
                multipart(&[("file", Some("over.md"), Some("text/markdown"), over.as_slice())]),
            )
            .await;
            assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
            assert_eq!(reply.json()["code"], "file_too_large");
        }
    });
}

#[test]
fn files_rudiments_upload_refusals_and_retry_by_id() {
    with_profile("files-refusals", |root| async move {
        let state = state_for(&profile_of(&root));
        // Viewers cannot write.
        let viewer = call(
            &state,
            "POST",
            &format!("/artifacts/{GRAPH}/files"),
            "viewer",
            Some(multipart_type().as_str()),
            multipart(&[("file", Some("a.txt"), Some("text/plain"), b"a".as_slice())]),
        )
        .await;
        assert_eq!(viewer.status, StatusCode::FORBIDDEN);

        let empty = upload(&state, "empty.txt", "text/plain", b"", &[]).await;
        assert_eq!(empty.status, StatusCode::BAD_REQUEST);
        assert_eq!(empty.json()["code"], "empty_file");

        let missing = call(
            &state,
            "POST",
            &format!("/artifacts/{GRAPH}/files"),
            "editor",
            Some(multipart_type().as_str()),
            multipart(&[("label", None, None, b"no file part".as_slice())]),
        )
        .await;
        assert_eq!(missing.status, StatusCode::BAD_REQUEST);
        assert_eq!(missing.json()["code"], "missing_file");

        let bad_id = upload(&state, "a.txt", "text/plain", b"a", &[("artifactId", "../escape")]).await;
        assert_eq!(bad_id.status, StatusCode::BAD_REQUEST);
        assert_eq!(bad_id.json()["code"], "invalid_artifact_id");

        let no_folder = upload(&state, "a.txt", "text/plain", b"a", &[("parentId", "folder-missing")]).await;
        assert_eq!(no_folder.status, StatusCode::NOT_FOUND);
        assert_eq!(no_folder.json()["code"], "folder_not_found");

        let first = upload(&state, "notes.txt", "text/plain", b"first bytes", &[("artifactId", "retry-me")]).await;
        assert_eq!(first.status, StatusCode::CREATED);
        let again = upload(&state, "notes.txt", "text/plain", b"first bytes", &[("artifactId", "retry-me")]).await;
        assert_eq!(again.status, StatusCode::OK, "{}", String::from_utf8_lossy(&again.body));
        assert_eq!(again.json()["replayed"], true);
        assert_eq!(again.json()["sha256"], first.json()["sha256"]);
        let different = upload(&state, "notes.txt", "text/plain", b"other bytes", &[("artifactId", "retry-me")]).await;
        assert_eq!(different.status, StatusCode::CONFLICT);
        assert_eq!(different.json()["code"], "artifact_exists");
        let download = get(&state, &format!("/artifacts/{GRAPH}/retry-me/download")).await;
        assert!(download.body == b"first bytes", "a refused retry replaced the bytes");
        // A plain-text upload is still a file, not a document: nothing parsed.
        let navigation = get(&state, &format!("/navigation/{GRAPH}")).await.json();
        assert!(navigation_artifact(&navigation, "retry-me").is_some());
        assert_eq!(navigation_artifact(&navigation, "retry-me").unwrap()["ingestedDocId"], Value::Null);
    });
}

#[test]
fn files_rudiments_rename_move_trash_restore_and_purge() {
    with_profile("files-lifecycle", |root| async move {
        let state = state_for(&profile_of(&root));
        let folder = send_json(
            &state,
            "PUT",
            &format!("/navigation/{GRAPH}/folders/folder-files"),
            json!({"label": "Files folder", "section": "artifacts"}),
        )
        .await;
        assert_eq!(folder.status, StatusCode::OK, "{}", String::from_utf8_lossy(&folder.body));
        let docs_folder = send_json(
            &state,
            "PUT",
            &format!("/navigation/{GRAPH}/folders/folder-docs"),
            json!({"label": "Docs folder"}),
        )
        .await;
        assert_eq!(docs_folder.status, StatusCode::OK);

        let bytes = binary_fixture(4096);
        let created = upload(&state, "report.zip", "application/zip", &bytes, &[("artifactId", "file-report")]).await;
        assert_eq!(created.status, StatusCode::CREATED);
        let order = created.json()["order"].clone();

        // Rename: only the label changes.
        let renamed = send_json(&state, "PATCH", &format!("/navigation/{GRAPH}/artifacts/file-report"), json!({"label": "  Q3 report.zip  "})).await;
        assert_eq!(renamed.status, StatusCode::OK, "{}", String::from_utf8_lossy(&renamed.body));
        assert_eq!(renamed.json()["label"], "Q3 report.zip");
        assert_eq!(renamed.json()["parentId"], Value::Null);
        assert_eq!(renamed.json()["order"], order);
        assert_eq!(renamed.json()["mimeType"], "application/zip");
        assert_eq!(renamed.json()["sizeBytes"].as_f64().unwrap() as usize, bytes.len());

        // Move: only the parent changes.
        let moved = send_json(&state, "PATCH", &format!("/navigation/{GRAPH}/artifacts/file-report"), json!({"parentId": "folder-files"})).await;
        assert_eq!(moved.status, StatusCode::OK);
        assert_eq!(moved.json()["parentId"], "folder-files");
        assert_eq!(moved.json()["label"], "Q3 report.zip");
        let navigation = get(&state, &format!("/navigation/{GRAPH}")).await.json();
        assert_eq!(navigation_artifact(&navigation, "file-report").unwrap()["parentId"], "folder-files");

        // Refusals.
        let into_docs = send_json(&state, "PATCH", &format!("/navigation/{GRAPH}/artifacts/file-report"), json!({"parentId": "folder-docs"})).await;
        assert_eq!(into_docs.status, StatusCode::NOT_FOUND);
        assert_eq!(into_docs.json()["code"], "folder_not_found");
        let nothing = send_json(&state, "PATCH", &format!("/navigation/{GRAPH}/artifacts/file-report"), json!({})).await;
        assert_eq!(nothing.status, StatusCode::BAD_REQUEST);
        assert_eq!(nothing.json()["code"], "invalid_patch");
        let blank = send_json(&state, "PATCH", &format!("/navigation/{GRAPH}/artifacts/file-report"), json!({"label": "   "})).await;
        assert_eq!(blank.json()["code"], "invalid_patch");
        let ghost = send_json(&state, "PATCH", &format!("/navigation/{GRAPH}/artifacts/file-ghost"), json!({"label": "x"})).await;
        assert_eq!(ghost.status, StatusCode::NOT_FOUND);
        assert_eq!(ghost.json()["code"], "artifact_not_found");

        // Trash: leaves the workspace, keeps the bytes.
        let trashed = call(&state, "DELETE", &format!("/navigation/{GRAPH}/artifacts/file-report"), "editor", None, Vec::new()).await;
        assert_eq!(trashed.status, StatusCode::OK, "{}", String::from_utf8_lossy(&trashed.body));
        assert_eq!(trashed.json()["status"], "trashed");
        assert_eq!(trashed.json()["trashed"], true);
        assert!(trashed.json()["trashedAt"].is_string());
        let navigation = get(&state, &format!("/navigation/{GRAPH}")).await.json();
        assert!(navigation_artifact(&navigation, "file-report").is_none());
        let again = call(&state, "DELETE", &format!("/navigation/{GRAPH}/artifacts/file-report"), "editor", None, Vec::new()).await;
        assert_eq!(again.status, StatusCode::OK);
        assert_eq!(again.json()["alreadyTrashed"], true);
        let trash = get(&state, &format!("/navigation/{GRAPH}/trash")).await;
        assert_eq!(trash.status, StatusCode::OK);
        let items = trash.json()["items"].as_array().unwrap().clone();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["artifactId"], "file-report");
        assert_eq!(items[0]["label"], "Q3 report.zip");
        assert_eq!(items[0]["parentId"], "folder-files");
        let preview = get(&state, &format!("/artifacts/{GRAPH}/file-report/download")).await;
        assert!(preview.body == bytes, "trashed bytes must stay readable until purge");

        // Restore: back into its folder, with its name.
        let graph_dir = crate::paths::existing_graph_dir(&state.app, GRAPH).unwrap();
        let trash_record = graph_dir.join("artifacts").join("file-report").join("trash.json");
        let trash_bytes = crate::storage::read_bytes(&trash_record).expect("trash record beside the bytes");
        let restored = call(&state, "POST", &format!("/navigation/{GRAPH}/trash/file-report/restore"), "editor", None, Vec::new()).await;
        assert_eq!(restored.status, StatusCode::OK, "{}", String::from_utf8_lossy(&restored.body));
        assert_eq!(restored.json()["restoredToParentId"], "folder-files");
        assert_eq!(restored.json()["label"], "Q3 report.zip");
        let navigation = get(&state, &format!("/navigation/{GRAPH}")).await.json();
        let back = navigation_artifact(&navigation, "file-report").expect("restored");
        assert_eq!(back["parentId"], "folder-files");
        assert_eq!(back["mimeType"], "application/zip");
        assert!(get(&state, &format!("/navigation/{GRAPH}/trash")).await.json()["items"].as_array().unwrap().is_empty());
        assert!(!trash_record.exists(), "restore clears the trash record");
        assert_eq!(
            call(&state, "DELETE", &format!("/navigation/{GRAPH}/trash/file-report"), "editor", None, Vec::new()).await.json()["code"],
            "not_in_trash"
        );
        // A crash between restore's two steps leaves a live file with a stale
        // trash record: it is not listed, cannot be purged, and the next
        // restore clears it.
        crate::storage::write_bytes(&trash_record, &trash_bytes).unwrap();
        assert!(get(&state, &format!("/navigation/{GRAPH}/trash")).await.json()["items"].as_array().unwrap().is_empty());
        let live = call(&state, "DELETE", &format!("/navigation/{GRAPH}/trash/file-report"), "editor", None, Vec::new()).await;
        assert_eq!(live.status, StatusCode::CONFLICT);
        assert_eq!(live.json()["code"], "artifact_live");
        let healed = call(&state, "POST", &format!("/navigation/{GRAPH}/trash/file-report/restore"), "editor", None, Vec::new()).await;
        assert_eq!(healed.status, StatusCode::OK);
        assert_eq!(healed.json()["alreadyLive"], true);
        assert!(!trash_record.exists());

        // Trash, delete its (now empty) folder, restore: it lands at the root.
        assert_eq!(call(&state, "DELETE", &format!("/navigation/{GRAPH}/artifacts/file-report"), "editor", None, Vec::new()).await.status, StatusCode::OK);
        let folder_gone = call(&state, "DELETE", &format!("/navigation/{GRAPH}/folders/folder-files"), "editor", None, Vec::new()).await;
        assert_eq!(folder_gone.status, StatusCode::OK, "{}", String::from_utf8_lossy(&folder_gone.body));
        let restored = call(&state, "POST", &format!("/navigation/{GRAPH}/trash/file-report/restore"), "editor", None, Vec::new()).await;
        assert_eq!(restored.status, StatusCode::OK);
        assert_eq!(restored.json()["restoredToParentId"], Value::Null);
        assert_eq!(restored.json()["parentId"], Value::Null);

        // Purge: gone for good.
        assert_eq!(call(&state, "DELETE", &format!("/navigation/{GRAPH}/artifacts/file-report"), "editor", None, Vec::new()).await.status, StatusCode::OK);
        let purged = call(&state, "DELETE", &format!("/navigation/{GRAPH}/trash/file-report"), "editor", None, Vec::new()).await;
        assert_eq!(purged.status, StatusCode::OK, "{}", String::from_utf8_lossy(&purged.body));
        assert_eq!(purged.json()["purged"], true);
        assert_eq!(get(&state, &format!("/artifacts/{GRAPH}/file-report/download")).await.status, StatusCode::NOT_FOUND);
        let not_in_trash = call(&state, "POST", &format!("/navigation/{GRAPH}/trash/file-report/restore"), "editor", None, Vec::new()).await;
        assert_eq!(not_in_trash.status, StatusCode::NOT_FOUND);
        assert_eq!(not_in_trash.json()["code"], "not_in_trash");
        assert!(!graph_dir.join("artifacts").join("file-report").exists());
        // A viewer cannot trash.
        let kept = upload(&state, "kept.txt", "text/plain", b"kept", &[("artifactId", "file-kept")]).await;
        assert_eq!(kept.status, StatusCode::CREATED);
        let denied = call(&state, "DELETE", &format!("/navigation/{GRAPH}/artifacts/file-kept"), "viewer", None, Vec::new()).await;
        assert_eq!(denied.status, StatusCode::FORBIDDEN);
    });
}

#[test]
fn files_rudiments_put_stays_a_full_replace_even_when_it_says_patch() {
    with_profile("files-put-replace", |root| async move {
        let state = state_for(&profile_of(&root));
        let folder = send_json(
            &state,
            "PUT",
            &format!("/navigation/{GRAPH}/folders/folder-files"),
            json!({"label": "Files folder", "section": "artifacts"}),
        )
        .await;
        assert_eq!(folder.status, StatusCode::OK, "{}", String::from_utf8_lossy(&folder.body));
        let created = upload(
            &state,
            "kept.zip",
            "application/zip",
            b"zip bytes",
            &[("artifactId", "file-put"), ("parentId", "folder-files")],
        )
        .await;
        assert_eq!(created.status, StatusCode::CREATED, "{}", String::from_utf8_lossy(&created.body));
        assert_eq!(created.json()["parentId"], "folder-files");

        // CONTRACT §5: the old PUT stays a full replace (absent fields are
        // cleared). `patch` is the cell's internal marker for the PATCH merge;
        // a PUT body that carries it must not become a merge.
        let put = send_json(
            &state,
            "PUT",
            &format!("/navigation/{GRAPH}/artifacts/file-put"),
            json!({"label": "Replaced.zip", "patch": true}),
        )
        .await;
        assert_eq!(put.status, StatusCode::OK, "{}", String::from_utf8_lossy(&put.body));
        let navigation = get(&state, &format!("/navigation/{GRAPH}")).await.json();
        let listed = navigation_artifact(&navigation, "file-put").expect("still listed");
        assert_eq!(listed["label"], "Replaced.zip");
        assert_eq!(listed["parentId"], Value::Null, "a PUT clears an absent parentId: {listed}");
        // A metadata PUT leaves the stored bytes alone.
        let download = get(&state, &format!("/artifacts/{GRAPH}/file-put/download")).await;
        assert!(download.body == b"zip bytes");
    });
}

// ── folder-view preferences ───────────────────────────────────────────────

/// JavaScript encodeURIComponent, independent of the cell's own encoder.
fn encode_component(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn view_record(incarnation: &str, presentation: &str) -> Value {
    let key = [OWNER, GRAPH, incarnation, "root", "files-default", "shared"]
        .iter()
        .map(|part| encode_component(part))
        .collect::<Vec<_>>()
        .join("/");
    json!({"kind":"FolderView","localId":format!("fv-{}", sha256_hex(key.as_bytes())),"schemaVersion":1,
        "ownerId":OWNER,"graphId":GRAPH,"graphIncarnation":incarnation,"folderKey":"root",
        "viewId":"files-default","audienceKind":"shared","presentation":presentation,"sort":"name",
        "direction":"asc","iconSize":"medium","snapToGrid":true})
}

fn placement_record(incarnation: &str, file_id: &str, x: f64) -> Value {
    let key = [OWNER, GRAPH, incarnation, "root", "files-default", "shared", "artifact", file_id]
        .iter()
        .map(|part| encode_component(part))
        .collect::<Vec<_>>()
        .join("/");
    json!({"kind":"FilePlacement","localId":format!("fp-{}", sha256_hex(key.as_bytes())),"schemaVersion":1,
        "ownerId":OWNER,"graphId":GRAPH,"graphIncarnation":incarnation,"folderKey":"root",
        "viewId":"files-default","audienceKind":"shared","fileKind":"artifact","fileId":file_id,
        "coordinateSpace":"folder-canvas","x":x,"y":24.0})
}

fn current_op(operation_id: &str, record: &Value, base: &str) -> Value {
    json!({"kind":"currentState","operationId":operation_id,"vocab":"garden-file-views",
        "class":record["kind"],"objectId":record["localId"],"baseVersion":base,"record":record})
}

fn face<'a>(views: &'a Value, object_id: &Value) -> &'a Value {
    views["currentState"]
        .as_array()
        .unwrap()
        .iter()
        .find(|face| &face["objectId"] == object_id)
        .unwrap_or_else(|| panic!("no face {object_id} in {views}"))
}

#[test]
fn files_rudiments_folder_view_preferences_round_trip() {
    with_profile("files-views", |root| async move {
        let state = state_for(&profile_of(&root));
        let path = format!("/navigation/{GRAPH}/file-views");
        let empty = get(&state, &path).await;
        assert_eq!(empty.status, StatusCode::OK, "{}", String::from_utf8_lossy(&empty.body));
        let empty = empty.json();
        assert_eq!(empty["available"], true);
        assert_eq!(empty["revision"], 0);
        assert!(empty["currentState"].as_array().unwrap().is_empty());
        let registry = empty["sourceRegistry"].as_array().unwrap();
        assert_eq!(registry.len(), 2, "{empty}");
        assert!(registry.iter().all(|entry| entry["vocab"] == "garden-file-views"
            && entry["storeTarget"] == "projection:file-views"
            && entry["reconciliationStrategy"] == "contested"
            && entry["sourceKind"] == "current-state"));
        let incarnation = empty["graphIncarnation"].as_str().unwrap().to_string();

        // The generic emporium writer cannot write this pack, before or after
        // preferences are stored (and preferences never activate the source
        // ledger: see the H1 cases below).
        let generic = crate::emporium_mcp_surface::mcp_local_emporium_write(
            state.app.clone(),
            &json!({"graphId":GRAPH,"vocab":"garden-file-views","records":[view_record(&incarnation, "canvas")]}),
        )
        .await;
        assert!(generic.is_err(), "inactive-ledger generic write: {generic:?}");

        let view = view_record(&incarnation, "canvas");
        let placement = placement_record(&incarnation, "file-a", 10.0);
        let pushed = crate::source_sync::mcp_local_source_push(
            state.app.clone(),
            &json!({"graphId":GRAPH,"graphIncarnation":incarnation,"operations":[
                current_op("fv-op-1", &view, "root"), current_op("fp-op-1", &placement, "root")]}),
        )
        .await
        .unwrap();
        assert_eq!(pushed["ok"], true, "{pushed}");
        let generic = crate::emporium_mcp_surface::mcp_local_emporium_write(
            state.app.clone(),
            &json!({"graphId":GRAPH,"vocab":"garden-file-views","records":[view_record(&incarnation, "list")]}),
        )
        .await;
        assert!(generic.unwrap_err().to_string().contains("source_push"));

        let views = get(&state, &path).await.json();
        assert!(views["revision"].as_u64().unwrap() >= 2);
        let view_face = face(&views, &view["localId"]);
        assert_eq!(view_face["record"]["presentation"], "canvas");
        assert_eq!(view_face["operationId"], "fv-op-1");
        assert!(view_face.get("conflictId").is_none());
        let placement_face = face(&views, &placement["localId"]).clone();
        assert_eq!(placement_face["record"]["x"], 10.0);
        let base = placement_face["sourceVersion"].as_str().unwrap().to_string();

        // Filters.
        let filtered = get(&state, &format!("{path}?folderKey=root&viewId=other-view")).await.json();
        assert!(filtered["currentState"].as_array().unwrap().is_empty());

        // An edit from the observed base moves the file.
        let moved = placement_record(&incarnation, "file-a", 99.0);
        let pushed = crate::source_sync::mcp_local_source_push(
            state.app.clone(),
            &json!({"graphId":GRAPH,"graphIncarnation":incarnation,"operations":[current_op("fp-op-2", &moved, &base)]}),
        )
        .await
        .unwrap();
        assert_eq!(pushed["ok"], true, "{pushed}");
        let views = get(&state, &path).await.json();
        let placement_face = face(&views, &placement["localId"]);
        assert_eq!(placement_face["record"]["x"], 99.0);
        assert!(placement_face.get("conflictId").is_none());

        // A write from a stale base is kept as a contest, not a silent overwrite.
        let stale = placement_record(&incarnation, "file-a", 5.0);
        crate::source_sync::mcp_local_source_push(
            state.app.clone(),
            &json!({"graphId":GRAPH,"graphIncarnation":incarnation,"operations":[current_op("fp-op-3", &stale, &base)]}),
        )
        .await
        .unwrap();
        let views = get(&state, &path).await.json();
        assert!(face(&views, &placement["localId"])["conflictId"].is_string(), "{views}");

        // A record from a retired incarnation is refused.
        let retired = placement_record("retired-incarnation", "file-b", 1.0);
        let refused = crate::source_sync::mcp_local_source_push(
            state.app.clone(),
            &json!({"graphId":GRAPH,"graphIncarnation":incarnation,"operations":[current_op("fp-op-4", &retired, "root")]}),
        )
        .await;
        assert!(refused.is_err() || refused.as_ref().is_ok_and(|value| value["ok"] == false));
    });
}

// ── folder-view preferences live outside the source ledger (H1, option c) ──
//
// Vera's ruling on H1 (2026-10-06): folder-view preferences are a small
// per-graph record, `{graph}/file-views/record.json`, outside the source ledger
// and outside `rebuild_source_projections`. A preference write must never
// activate source authority and never trigger a projection rebuild, so it can
// no longer rewind writers that bypass the ledger (`sparql_update`, `rdf_load`,
// `revaluate`, user-value writes). Written red against `162b8f1`, where the
// first preference write activates the ledger and every later one restores
// its checkpoint.

fn graph_dir_of(state: &Arc<LoopbackState>) -> std::path::PathBuf {
    crate::paths::existing_graph_dir(&state.app, GRAPH).unwrap()
}

/// Where the record lives inside a graph directory (CONTRACT-files §7).
fn preferences_record_path(graph_dir: &std::path::Path) -> std::path::PathBuf {
    graph_dir.join("file-views").join("record.json")
}

fn source_ledger_path(graph_dir: &std::path::Path) -> std::path::PathBuf {
    graph_dir.join("source-sync").join("ledger.json")
}

async fn file_views(state: &Arc<LoopbackState>) -> Value {
    let reply = get(state, &format!("/navigation/{GRAPH}/file-views")).await;
    assert_eq!(
        reply.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    reply.json()
}

async fn live_incarnation(state: &Arc<LoopbackState>) -> String {
    file_views(state).await["graphIncarnation"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn push_views(
    state: &Arc<LoopbackState>,
    incarnation: &str,
    operations: Value,
) -> crate::app_error::AppResult<Value> {
    crate::source_sync::mcp_local_source_push(
        state.app.clone(),
        &json!({"graphId": GRAPH, "graphIncarnation": incarnation, "operations": operations}),
    )
    .await
}

/// Put the graph under source authority the ordinary way: the offline mirror's
/// first `source_pull` writes the ledger and its checkpoint. This is how a graph
/// could already be under source authority before any preference is written.
async fn activate_source_authority(state: &Arc<LoopbackState>) {
    crate::source_sync::mcp_local_source_pull(state.app.clone(), &json!({"graphId": GRAPH}))
        .await
        .expect("source_pull activates the source ledger");
    assert!(crate::source_sync::source_authority_active(&state.app, GRAPH).unwrap());
}

/// A writer that bypasses the source ledger.
async fn direct_rdf_write(state: &Arc<LoopbackState>, value: &str) {
    crate::rdf_service::mcp_local_sparql_update(
        state.app.clone(),
        &json!({"graphId": GRAPH, "update": format!(
            "INSERT DATA {{ GRAPH <urn:files-rudiments:h1> {{ <urn:files-rudiments:s> <urn:files-rudiments:p> \"{value}\" }} }}"
        )}),
    )
    .await
    .expect("direct RDF write");
}

async fn direct_rdf_values(state: &Arc<LoopbackState>) -> String {
    crate::rdf_service::mcp_local_sparql_query(
        state.app.clone(),
        &json!({"graphId": GRAPH, "query":
            "SELECT ?o WHERE { GRAPH <urn:files-rudiments:h1> { <urn:files-rudiments:s> <urn:files-rudiments:p> ?o } }"}),
    )
    .await
    .unwrap()
    .to_string()
}

fn sorted_nquads(graph_dir: &std::path::Path) -> Vec<String> {
    let store = crate::rdf_service::open_graph_store(graph_dir).unwrap();
    let dump = crate::rdf_query_service::dump_rdf_from_store(&store, "nquads", None, None).unwrap();
    let mut lines = dump.data.lines().map(str::to_string).collect::<Vec<_>>();
    lines.sort();
    lines
}

/// Every `rebuild_source_projections` ends with a seed walk over the restored
/// store (`SeedReplayGuard::finish`), which the seed service counts per store.
fn projection_rebuilds(graph_dir: &std::path::Path) -> u64 {
    crate::rdf_seed_service::reseed_count(&graph_dir.join("store.oxigraph"))
}

fn receipt<'a>(pushed: &'a Value, operation_id: &str) -> &'a Value {
    pushed["receipts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|receipt| receipt["operationId"] == operation_id)
        .unwrap_or_else(|| panic!("no receipt {operation_id} in {pushed}"))
}

/// Test 1 of the H1 ruling: a preference write leaves the source ledger
/// untouched, whether or not the graph was already under source authority, and
/// a batch that mixes preferences with ledger operations is refused whole.
#[test]
fn files_rudiments_folder_view_write_leaves_the_source_ledger_untouched() {
    with_profile("files-views-ledger", |root| async move {
        let state = state_for(&profile_of(&root));
        let graph_dir = graph_dir_of(&state);
        let incarnation = live_incarnation(&state).await;
        assert!(!source_ledger_path(&graph_dir).exists());

        let view = view_record(&incarnation, "canvas");
        let placement = placement_record(&incarnation, "file-a", 10.0);
        let pushed = push_views(
            &state,
            &incarnation,
            json!([current_op("fv-ledger-1", &view, "root"), current_op("fp-ledger-1", &placement, "root")]),
        )
        .await
        .unwrap();
        assert_eq!(pushed["ok"], true, "{pushed}");
        assert!(
            !source_ledger_path(&graph_dir).exists(),
            "a preference write created the source ledger"
        );
        assert!(!crate::source_sync::source_authority_active(&state.app, GRAPH).unwrap());
        assert!(
            preferences_record_path(&graph_dir).is_file(),
            "the preferences record is not at {{graph}}/file-views/record.json"
        );

        // Mixed with a ledger operation: refused whole, nothing stored anywhere.
        let record_before = std::fs::read(preferences_record_path(&graph_dir)).unwrap();
        let mixed = push_views(
            &state,
            &incarnation,
            json!([
                current_op("fp-mixed-1", &placement_record(&incarnation, "file-mixed", 1.0), "root"),
                {"kind": "graphMetadata", "operationId": "meta-mixed-1", "title": "Mixed batch"}
            ]),
        )
        .await;
        assert!(mixed.is_err(), "a mixed batch was accepted: {mixed:?}");
        assert!(!source_ledger_path(&graph_dir).exists());
        assert_eq!(std::fs::read(preferences_record_path(&graph_dir)).unwrap(), record_before);

        // A graph already under source authority: the ledger bytes do not move.
        activate_source_authority(&state).await;
        let ledger_before = std::fs::read(source_ledger_path(&graph_dir)).unwrap();
        let pushed = push_views(
            &state,
            &incarnation,
            json!([current_op("fp-ledger-2", &placement_record(&incarnation, "file-b", 30.0), "root")]),
        )
        .await
        .unwrap();
        assert_eq!(pushed["ok"], true, "{pushed}");
        assert_eq!(
            std::fs::read(source_ledger_path(&graph_dir)).unwrap(),
            ledger_before,
            "a preference write changed the source ledger"
        );
        let views = file_views(&state).await;
        assert_eq!(face(&views, &placement_record(&incarnation, "file-b", 30.0)["localId"])["record"]["x"], 30.0);
    });
}

/// Test 2 of the H1 ruling, and the former `#[ignore]`d H1 witness turned into
/// a regression test (expected to PASS): a direct RDF write made after a
/// preference write survives the next preference write, on a fresh graph and
/// on a graph that was already under source authority.
#[test]
fn files_rudiments_h1_folder_view_push_keeps_later_direct_rdf_writes() {
    with_profile("files-views-h1", |root| async move {
        let state = state_for(&profile_of(&root));
        let path = format!("/navigation/{GRAPH}/file-views");
        let incarnation = live_incarnation(&state).await;
        let canvas = view_record(&incarnation, "canvas");
        let first = push_views(&state, &incarnation, json!([current_op("h1-op-1", &canvas, "root")]))
            .await
            .unwrap();
        assert_eq!(first["ok"], true, "{first}");

        // An ordinary direct RDF write, made after the first preference write.
        direct_rdf_write(&state, "h1-witness-value").await;
        let before = direct_rdf_values(&state).await;
        assert!(before.contains("h1-witness-value"), "the direct write did not land: {before}");

        // The next folder-view write (a drag), from the observed base.
        let base = face(&get(&state, &path).await.json(), &canvas["localId"])["sourceVersion"]
            .as_str()
            .unwrap()
            .to_string();
        let second = push_views(
            &state,
            &incarnation,
            json!([current_op("h1-op-2", &view_record(&incarnation, "list"), &base)]),
        )
        .await
        .unwrap();
        assert_eq!(second["ok"], true, "{second}");
        let after = direct_rdf_values(&state).await;
        assert!(
            after.contains("h1-witness-value"),
            "H1: a folder-view push rewound a direct RDF write made after the first one: {after}"
        );

        // The same on a graph that was already under source authority (the
        // pre-existing hazard (b) is out of scope; preferences must not set it off).
        activate_source_authority(&state).await;
        direct_rdf_write(&state, "h1-after-activation").await;
        let third = push_views(
            &state,
            &incarnation,
            json!([current_op("h1-op-3", &placement_record(&incarnation, "file-h1", 7.0), "root")]),
        )
        .await
        .unwrap();
        assert_eq!(third["ok"], true, "{third}");
        let after = direct_rdf_values(&state).await;
        assert!(
            after.contains("h1-after-activation") && after.contains("h1-witness-value"),
            "H1: a folder-view push rewound a direct RDF write on a graph under source authority: {after}"
        );
    });
}

/// Test 3 of the H1 ruling: a preference write costs no projection rebuild and
/// leaves the graph's RDF exactly as it was (preferences have no RDF face).
#[test]
fn files_rudiments_folder_view_write_costs_no_projection_rebuild() {
    with_profile("files-views-rebuild", |root| async move {
        let state = state_for(&profile_of(&root));
        let graph_dir = graph_dir_of(&state);
        let incarnation = live_incarnation(&state).await;
        // Warm the store the ordinary way, so a later walk can only come from a rebuild.
        direct_rdf_values(&state).await;

        for (phase, operation_id, file_id) in [
            ("fresh graph", "fp-rebuild-1", "file-r1"),
            ("graph under source authority", "fp-rebuild-2", "file-r2"),
        ] {
            if phase == "graph under source authority" {
                activate_source_authority(&state).await;
            }
            let rebuilds_before = projection_rebuilds(&graph_dir);
            let rdf_before = sorted_nquads(&graph_dir);
            let pushed = push_views(
                &state,
                &incarnation,
                json!([current_op(operation_id, &placement_record(&incarnation, file_id, 5.0), "root")]),
            )
            .await
            .unwrap();
            assert_eq!(pushed["ok"], true, "{phase}: {pushed}");
            assert!(
                pushed.get("projection").is_none_or(Value::is_null),
                "{phase}: the preference write ran a projection rebuild: {}",
                pushed["projection"]
            );
            assert_eq!(
                projection_rebuilds(&graph_dir),
                rebuilds_before,
                "{phase}: the preference write re-walked the projections"
            );
            assert!(
                sorted_nquads(&graph_dir) == rdf_before,
                "{phase}: the preference write changed the graph's RDF"
            );
        }
    });
}

/// Test 4 of the H1 ruling: preferences survive a cell restart, in the same
/// profile and through the hosted durable flush and hydrate, and the observed
/// version read before the restart is still a valid base after it.
#[test]
fn files_rudiments_folder_view_preferences_survive_a_cell_restart() {
    with_profile("files-views-restart", |root| async move {
        let profile = profile_of(&root);
        let (incarnation, before) = {
            let state = state_for(&profile);
            let incarnation = live_incarnation(&state).await;
            let pushed = push_views(
                &state,
                &incarnation,
                json!([
                    current_op("fv-restart-1", &view_record(&incarnation, "canvas"), "root"),
                    current_op("fp-restart-1", &placement_record(&incarnation, "file-kept", 64.0), "root")
                ]),
            )
            .await
            .unwrap();
            assert_eq!(pushed["ok"], true, "{pushed}");
            (incarnation, file_views(&state).await)
        };
        assert_eq!(before["currentState"].as_array().unwrap().len(), 2, "{before}");

        // Same profile, fresh runtime state.
        {
            let state = state_for(&profile);
            let views = file_views(&state).await;
            assert_eq!(views["currentState"], before["currentState"]);
            assert_eq!(views["revision"], before["revision"]);
        }

        // The hosted shape: flush to the durable plane, hydrate a new profile.
        let durable = root.join("durable");
        let flushed = crate::cell_durability::flush(&profile, &durable).expect("durable flush");
        assert!(flushed.published);
        let fresh = root.join("profile-after-restart");
        assert!(crate::cell_durability::hydrate(&fresh, &durable).expect("hydrate"));
        std::env::set_var("GARDEN_PROFILE_DIR", &fresh);
        let state = state_for(&fresh);
        assert!(
            preferences_record_path(&fresh.join("graphs").join(GRAPH)).is_file(),
            "the hydrated profile has no preferences record"
        );
        let views = file_views(&state).await;
        assert_eq!(views["graphIncarnation"], before["graphIncarnation"]);
        assert_eq!(views["currentState"], before["currentState"]);
        assert_eq!(views["revision"], before["revision"]);

        // A write from the version observed before the restart is applied.
        let placement = placement_record(&incarnation, "file-kept", 65.0);
        let base = face(&before, &placement["localId"])["sourceVersion"]
            .as_str()
            .unwrap()
            .to_string();
        let pushed = push_views(&state, &incarnation, json!([current_op("fp-restart-2", &placement, &base)]))
            .await
            .unwrap();
        assert_eq!(receipt(&pushed, "fp-restart-2")["outcome"]["outcome"], "applied", "{pushed}");
        let views = file_views(&state).await;
        let moved = face(&views, &placement["localId"]);
        assert_eq!(moved["record"]["x"], 65.0);
        assert!(moved.get("conflictId").is_none(), "{views}");
    });
}

/// Test 5 of the H1 ruling: two writes from one observed version resolve as
/// the contract says. Launched concurrently: one is applied, the other is
/// kept as a contest with both candidates; a retry replays its receipt; a
/// reused operation id with other content and a retired incarnation are
/// rejected; a resolution chooses a candidate and a write from the resolved
/// head is applied.
#[test]
fn files_rudiments_folder_view_concurrent_writes_resolve_as_the_contract_says() {
    with_profile("files-views-concurrent", |root| async move {
        let state = state_for(&profile_of(&root));
        let graph_dir = graph_dir_of(&state);
        let incarnation = live_incarnation(&state).await;
        let start = placement_record(&incarnation, "file-c", 0.0);
        let object_id = start["localId"].clone();
        push_views(&state, &incarnation, json!([current_op("fp-c-0", &start, "root")]))
            .await
            .unwrap();
        let observed = face(&file_views(&state).await, &object_id)["sourceVersion"]
            .as_str()
            .unwrap()
            .to_string();

        let a = placement_record(&incarnation, "file-c", 100.0);
        let b = placement_record(&incarnation, "file-c", 200.0);
        let (pushed_a, pushed_b) = tokio::join!(
            push_views(&state, &incarnation, json!([current_op("fp-c-a", &a, &observed)])),
            push_views(&state, &incarnation, json!([current_op("fp-c-b", &b, &observed)])),
        );
        let (pushed_a, pushed_b) = (pushed_a.unwrap(), pushed_b.unwrap());
        let mut outcomes = vec![
            receipt(&pushed_a, "fp-c-a")["outcome"]["outcome"].as_str().unwrap().to_string(),
            receipt(&pushed_b, "fp-c-b")["outcome"]["outcome"].as_str().unwrap().to_string(),
        ];
        outcomes.sort();
        assert_eq!(outcomes, vec!["applied", "conflict"], "{pushed_a} {pushed_b}");

        let views = file_views(&state).await;
        let contested = face(&views, &object_id).clone();
        let conflict_id = contested["conflictId"].as_str().expect("a contest").to_string();
        let conflict = views["conflicts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|conflict| conflict["conflictId"] == conflict_id.as_str())
            .unwrap_or_else(|| panic!("no conflict {conflict_id} in {views}"));
        let mut candidates = conflict["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|candidate| candidate["operationId"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        candidates.sort();
        assert_eq!(candidates, vec!["fp-c-a", "fp-c-b"]);

        // A retry replays the stored receipt; nothing is written twice.
        let retried = push_views(&state, &incarnation, json!([current_op("fp-c-a", &a, &observed)]))
            .await
            .unwrap();
        assert_eq!(receipt(&retried, "fp-c-a")["duplicate"], true, "{retried}");
        assert_eq!(receipt(&retried, "fp-c-a")["outcome"], receipt(&pushed_a, "fp-c-a")["outcome"]);
        assert_eq!(retried["revision"], views["revision"]);

        // Rejected: a reused operation id with other content; a retired incarnation.
        let reused = push_views(
            &state,
            &incarnation,
            json!([current_op("fp-c-a", &placement_record(&incarnation, "file-c", 300.0), &observed)]),
        )
        .await
        .unwrap_err();
        assert_eq!(reused.code(), Some("operation_id_reused"), "{reused}");
        let retired = push_views(
            &state,
            "a-retired-incarnation",
            json!([current_op("fp-c-z", &placement_record("a-retired-incarnation", "file-c", 1.0), &observed)]),
        )
        .await
        .unwrap_err();
        assert_eq!(retired.code(), Some("stale_graph_incarnation"), "{retired}");

        // A resolution chooses a candidate; the next write from that head applies.
        let resolved = push_views(
            &state,
            &incarnation,
            json!([{"kind": "resolveCurrent", "operationId": "fp-c-resolve", "objectKey": contested["objectKey"],
                    "conflictId": conflict_id, "chosenOperationId": "fp-c-b"}]),
        )
        .await
        .unwrap();
        assert_eq!(receipt(&resolved, "fp-c-resolve")["outcome"]["outcome"], "resolved", "{resolved}");
        let views = file_views(&state).await;
        let head = face(&views, &object_id).clone();
        assert!(head.get("conflictId").is_none(), "{views}");
        assert_eq!(head["record"]["x"], 200.0);
        assert_eq!(head["operationId"], "fp-c-resolve");
        let next = push_views(
            &state,
            &incarnation,
            json!([current_op("fp-c-next", &placement_record(&incarnation, "file-c", 250.0), head["sourceVersion"].as_str().unwrap())]),
        )
        .await
        .unwrap();
        assert_eq!(receipt(&next, "fp-c-next")["outcome"]["outcome"], "applied", "{next}");
        assert!(!source_ledger_path(&graph_dir).exists(), "preferences activated the source ledger");
    });
}

/// One ledger row as `source_push` stored it on the 10-02 prototype candidate
/// (`afa6d21`): the operation as serde writes it (absent options as null) and
/// its digest, the SHA-256 of the operation's canonical JSON.
fn legacy_ledger_row(operation_id: &str, record: &Value, accepted_revision: u64) -> Value {
    let mut operation = current_op(operation_id, record, "root");
    operation["causalOrder"] = Value::Null;
    operation["clientId"] = Value::Null;
    operation["evidenceWeight"] = Value::Null;
    let digest = sha256_hex(&serde_json_canonicalizer::to_vec(&operation).unwrap());
    json!({"digest": digest, "acceptedRevision": accepted_revision, "status": "applied",
        "operation": operation, "outcome": {"outcome": "applied"}})
}

/// Test 6 of the H1 ruling: a graph that already holds folder-view
/// preferences in the source ledger (written by the 10-02 prototype
/// candidate) reads them, a retry of one of them is recognised, a write from
/// one of their versions is applied, and the ledger itself is not touched.
#[test]
fn files_rudiments_folder_view_reads_preferences_left_in_the_source_ledger() {
    with_profile("files-views-legacy", |root| async move {
        let state = state_for(&profile_of(&root));
        let graph_dir = graph_dir_of(&state);
        let incarnation = live_incarnation(&state).await;
        let view = view_record(&incarnation, "canvas");
        let placement = placement_record(&incarnation, "file-legacy", 42.0);
        let ledger = json!({"schemaVersion": 1, "graphId": GRAPH, "graphIncarnation": incarnation, "revision": 2,
            "operations": {"legacy-fv-1": legacy_ledger_row("legacy-fv-1", &view, 1),
                           "legacy-fp-1": legacy_ledger_row("legacy-fp-1", &placement, 2)},
            "workflowFoldTriples": []});
        crate::storage::write_bytes(
            &source_ledger_path(&graph_dir),
            &serde_json::to_vec_pretty(&ledger).unwrap(),
        )
        .unwrap();
        let ledger_bytes = std::fs::read(source_ledger_path(&graph_dir)).unwrap();

        let views = file_views(&state).await;
        assert_eq!(views["revision"], 2, "{views}");
        assert_eq!(face(&views, &view["localId"])["record"]["presentation"], "canvas");
        assert_eq!(face(&views, &view["localId"])["operationId"], "legacy-fv-1");
        let legacy = face(&views, &placement["localId"]).clone();
        assert_eq!(legacy["record"]["x"], 42.0);
        assert_eq!(legacy["operationId"], "legacy-fp-1");

        // The prototype's retry is recognised as the same operation.
        let retried = push_views(&state, &incarnation, json!([current_op("legacy-fp-1", &placement, "root")]))
            .await
            .unwrap();
        assert_eq!(receipt(&retried, "legacy-fp-1")["duplicate"], true, "{retried}");

        // A write from the legacy version is applied, not contested.
        let moved = placement_record(&incarnation, "file-legacy", 43.0);
        let pushed = push_views(
            &state,
            &incarnation,
            json!([current_op("after-legacy-1", &moved, legacy["sourceVersion"].as_str().unwrap())]),
        )
        .await
        .unwrap();
        assert_eq!(receipt(&pushed, "after-legacy-1")["outcome"]["outcome"], "applied", "{pushed}");
        let views = file_views(&state).await;
        assert_eq!(views["revision"], 3, "{views}");
        assert_eq!(face(&views, &placement["localId"])["record"]["x"], 43.0);
        assert!(face(&views, &placement["localId"]).get("conflictId").is_none(), "{views}");
        assert_eq!(face(&views, &view["localId"])["record"]["presentation"], "canvas");
        assert_eq!(
            std::fs::read(source_ledger_path(&graph_dir)).unwrap(),
            ledger_bytes,
            "reading or extending legacy preferences changed the source ledger"
        );
        assert!(preferences_record_path(&graph_dir).is_file());
    });
}

// ── a cell restart keeps the bytes ─────────────────────────────────────────

#[test]
fn files_rudiments_bytes_survive_reopen_and_durable_round_trip() {
    with_profile("files-restart", |root| async move {
        let profile = profile_of(&root);
        let bytes = binary_fixture(1_048_577);
        let id = {
            let state = state_for(&profile);
            let reply = upload(&state, "kept.tar", "application/x-tar", &bytes, &[]).await;
            assert_eq!(reply.status, StatusCode::CREATED);
            reply.json()["id"].as_str().unwrap().to_string()
        };
        // Same profile, fresh runtime state.
        {
            let state = state_for(&profile);
            let download = get(&state, &format!("/artifacts/{GRAPH}/{id}/download")).await;
            assert_eq!(download.status, StatusCode::OK);
            assert!(download.body == bytes);
        }
        // The hosted shape: flush to the durable plane, hydrate a new profile
        // (what a restarted pod does), and read the file back from there.
        let durable = root.join("durable");
        let flushed = crate::cell_durability::flush(&profile, &durable).expect("durable flush");
        assert!(flushed.published);
        let fresh = root.join("profile-after-restart");
        assert!(crate::cell_durability::hydrate(&fresh, &durable).expect("hydrate"));
        std::env::set_var("GARDEN_PROFILE_DIR", &fresh);
        let state = state_for(&fresh);
        let download = get(&state, &format!("/artifacts/{GRAPH}/{id}/download")).await;
        assert_eq!(download.status, StatusCode::OK);
        assert!(download.body == bytes, "bytes differ after the durable round trip");
        let navigation = get(&state, &format!("/navigation/{GRAPH}")).await.json();
        assert!(navigation_artifact(&navigation, &id).is_some(), "the file is listed after restart");
    });
}
