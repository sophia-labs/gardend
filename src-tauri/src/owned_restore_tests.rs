//! Real signed Axum route -> durable job -> CRDT queue -> persistence.
//! The independent Platform fixture is synthetic, not production migration.
use super::*;
use axum::{body::Body, http::Request};
use base64::{engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD}, Engine};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use tower::ServiceExt;
use crate::local_jobs::{LocalJobRegistry, LocalJobStatus};
use crate::loopback_state::LoopbackManifest;

const GRAPH: &str = "demi-cross-format-fixture-20260908";
const OWNER: &str = "user:fixture-cloud1-owner-20260908";
const SECRET: &[u8] = b"owned-restore-test-secret-at-least-32-bytes";
const TOKEN: &str = "owned-restore-disposable-token";

#[cfg(test)]
mod tests {
use super::*;
async fn legacy_get(state: Arc<LoopbackState>, path: &str, lease: Option<&str>)
    -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut request=Request::builder().method("GET").uri(path)
        .header("authorization",format!("Bearer {TOKEN}"));
    if let Some(lease)=lease {request=request.header("x-sophia-cell-lease",lease)}
    let response=crate::loopback_router::loopback_router(state).oneshot(request.body(Body::empty()).unwrap()).await.unwrap();
    let status=response.status();let headers=response.headers().clone();
    let bytes=axum::body::to_bytes(response.into_body(),64*1024*1024).await.unwrap().to_vec();
    (status,headers,bytes)
}

fn legacy_history_api_trial(reopen: bool) {
    let _serial=crate::tauri_runtime::profile_env_serial().lock().unwrap_or_else(|p|p.into_inner());
    let input=std::path::PathBuf::from(std::env::var("GARDEN_PRESERVATION_HISTORY_READBACK").unwrap());
    let corpus:Value=serde_json::from_slice(&std::fs::read(&input).unwrap()).unwrap();
    let profiles=input.parent().unwrap().join("owner-api-profiles");
    let mut observations=Vec::new();
    let outcome=std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap().block_on(async {
            // Mixed originals/history/unavailable source fixtures plus a source orphan.
            let selected=corpus.as_array().unwrap().iter().filter(|row| {
                matches!(row["input"]["graphId"].as_str(),Some("vera"|"nixon"|"marcy"))
                    || (row["input"]["graphId"]=="onboarding" && row["input"]["sourceOwner"]=="e9e949fe-0091-7015-0ab8-10bf259084ab")
            }).collect::<Vec<_>>();
            assert_eq!(selected.len(),4);
            for (number,row) in selected.iter().enumerate() {
                let graph=row["input"]["graphId"].as_str().unwrap();
                let user=row["input"]["sourceOwner"].as_str().unwrap();let owner=format!("user:{user}");
                let profile=profiles.join(number.to_string());
                std::env::set_var("GARDEN_PROFILE_DIR",&profile);
                let state=state_for(&profile,&owner,graph);
                if !reopen {
                    crate::graph_service::create_graph_service(&state.app,crate::graph_service::CreateGraphInput {
                        graph_id:Some(graph.into()),title:"Legacy readback fixture".into(),description:None,operation_id:None,
                    }).unwrap();
                }
                let dir=crate::paths::existing_graph_dir(&state.app,graph).unwrap();
                let root=dir.join(".migration/preservation-v2");
                let source=std::path::PathBuf::from(row["historyOnlyGraphDirectory"].as_str().unwrap());
                if !reopen {
                    for name in ["document-history.json","materialization-complete.json","source/history/documents/index.json"] {
                        let destination=root.join(name);std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
                        std::fs::write(destination,std::fs::read(source.join(".migration/preservation-v2").join(name)).unwrap()).unwrap();
                    }
                    for entry in row["disposition"]["entries"].as_array().unwrap() {
                        if let Some(member)=entry["sourceMember"].as_str() {
                            let bytes=crate::document_legacy_history::source_bytes(&source,entry).unwrap();
                            let path=root.join("source").join(member);std::fs::create_dir_all(path.parent().unwrap()).unwrap();std::fs::write(path,bytes).unwrap();
                        }
                    }
                }
                let token=lease_for(&owner,graph,"owner",None);
                let path=format!("/v1/graphs/{graph}/legacy-history");
                for role in ["viewer","editor"] {
                    let denied=lease_for(&owner,graph,role,None);
                    assert_eq!(legacy_get(state.clone(),&path,Some(&denied)).await.0,StatusCode::FORBIDDEN);
                }
                assert!(!legacy_get(state.clone(),&path,None).await.0.is_success());
                let wrong=lease_for(&owner,graph,"owner",Some(("sub",json!("user:foreign-owner"))));
                assert!(!legacy_get(state.clone(),&path,Some(&wrong)).await.0.is_success());
                let (status,_,body)=legacy_get(state.clone(),&path,Some(&token)).await;
                assert_eq!(status,StatusCode::OK,"owner discovery status");
                let mut discovery:Value=serde_json::from_slice(&body).unwrap();
                assert!(discovery["page"]["nextCursor"].is_null());discovery.as_object_mut().unwrap().remove("page");
                assert_eq!(discovery,row["disposition"]);
                let mut returned=0;
                for entry in discovery["entries"].as_array().unwrap() {
                    let doc=entry["documentId"].as_str().unwrap();let id=entry["snapshotId"].as_str().unwrap();
                    let record=format!("/v1/documents/{graph}/{doc}/legacy-history/{id}");
                    let (status,_,body)=legacy_get(state.clone(),&record,Some(&token)).await;
                    assert_eq!(status,StatusCode::OK);assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(),*entry);
                    let (status,headers,body)=legacy_get(state.clone(),&format!("{record}/download"),Some(&token)).await;
                    if entry["status"]=="source-payload-unavailable" {assert_eq!(status,StatusCode::GONE);continue}
                    assert_eq!(status,StatusCode::OK);assert_eq!(headers["x-content-type-options"],"nosniff");
                    assert!(headers["content-disposition"].to_str().unwrap().starts_with("attachment;"));
                    let expected=crate::document_legacy_history::source_bytes(&source,entry).unwrap();
                    assert!(body==expected,"download bytes differ");returned+=1;
                    let (status,headers,text)=legacy_get(state.clone(),&format!("{record}/text"),Some(&token)).await;
                    assert_eq!(status,StatusCode::OK);assert_eq!(headers["content-type"],"text/plain; charset=utf-8");
                    assert_eq!(headers["x-content-type-options"],"nosniff");
                    assert!(text==crate::document_legacy_history::literal_text(&expected).unwrap().into_bytes(),"literal text differs");
                    let response=crate::loopback_router::loopback_router(state.clone()).oneshot(Request::builder()
                        .method("POST").uri(format!("{record}/restore")).header("authorization",format!("Bearer {TOKEN}"))
                        .header("x-sophia-cell-lease",&token).body(Body::empty()).unwrap()).await.unwrap();
                    assert!(!response.status().is_success(),"legacy restore must not exist");
                }
                assert!(crate::document_service::list_documents(state.app.clone(),graph.into()).unwrap().is_empty(),"readback invented current documents");
                observations.push(json!({"sourceOwner":user,"graphId":graph,"returnedPayloads":returned,
                    "discoveredEntries":discovery["entries"].as_array().unwrap().len(),"reopen":reopen,
                    "signedRouter":true,"networkSocket":false,"currentDocumentsCreated":0}));
            }
        });
    }));
    std::env::remove_var("GARDEN_PROFILE_DIR");
    if let Err(error)=outcome {std::panic::resume_unwind(error)}
    let output=std::path::PathBuf::from(std::env::var("GARDEN_PRESERVATION_PREPARE_OUTPUT").unwrap());
    std::fs::create_dir_all(&output).unwrap();
    std::fs::write(output.join(if reopen{"legacy-api-reopen.json"}else{"legacy-api-first.json"}),serde_json::to_vec_pretty(&observations).unwrap()).unwrap();
}

#[test]
fn migration_legacy_history_api_first() {legacy_history_api_trial(false)}
#[test]
fn migration_legacy_history_api_reopen() {legacy_history_api_trial(true)}
}

struct RestoreTestLogger;
static RESTORE_TEST_LOGGER: RestoreTestLogger = RestoreTestLogger;
impl log::Log for RestoreTestLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool { metadata.level() <= log::Level::Warn }
    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) { eprintln!("restore-test {}: {}", record.target(), record.args()); }
    }
    fn flush(&self) {}
}

fn lease(role: &str, altered: Option<(&str, Value)>) -> String {
    lease_for(OWNER, GRAPH, role, altered)
}

fn lease_for(owner: &str, graph: &str, role: &str, altered: Option<(&str, Value)>) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let mut claims = json!({"iss":"pn-gateway","aud":"gardend-cell","sub":owner,"owner":owner,
        "graphId":graph,"generation":1,"cellId":bound_cell_id(owner,graph,1),"role":role,
        "policyRevision":1,"registryRevision":1,"sessionId":"owned-restore-test","iat":now,"exp":now+600});
    if let Some((key,value)) = altered { claims[key] = value; }
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
    let signed = format!("{header}.{payload}");
    let mut mac = Hmac::<Sha256>::new_from_slice(SECRET).unwrap();
    mac.update(signed.as_bytes());
    format!("{signed}.{}", URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
}

fn state(profile: &std::path::Path) -> Arc<LoopbackState> {
    state_for(profile, OWNER, GRAPH)
}

fn state_for(profile: &std::path::Path, owner: &str, graph: &str) -> Arc<LoopbackState> {
    let app = crate::tauri_runtime::build_mock_cell_app_for_tests(graph, owner, 1);
    let boundary = Arc::new(CellGraphBoundary::new_with_binding(Some(graph.into()),Some(owner.into()),Some(1),Some(1),Some(SECRET.to_vec())).unwrap());
    let jobs = Arc::new(LocalJobRegistry::new_bound(profile.join("jobs"), &boundary).unwrap());
    let grant = crate::loopback_token_grants::session_all_token_grant();
    let manifest = LoopbackManifest {
        runtime_profile:"test",bind_host:"127.0.0.1",port:0,api_url:"test".into(),mcp_url:"test".into(),openapi_url:"test".into(),token:TOKEN.into(),pid:0,
        started_at:"test".into(),manifest_path:"test".into(),auth_header:"test",token_audience:"test",token_storage:"test",security_warning:"test",
        cell_graph_id:Some(graph.into()),cell_owner:Some(owner.into()),cell_generation:Some(1),cell_registry_revision:Some(1),
        capabilities:grant.scopes.clone(),token_scope_mode:grant.scope_mode,token_scopes:grant.scopes,scope_details:grant.scope_details,grant_profiles:grant.grant_profiles,
    };
    Arc::new(LoopbackState { app,token:TOKEN.into(),manifest,jobs,
        services:Arc::new(crate::local_service_host::LocalServiceHost::default()),
        lifecycle:Arc::new(crate::cell_lifecycle::CellLifecycle::new()),
        cell_graph:boundary,
    })
}

fn multipart(fixture: &Value, altered: Option<(&str, Value)>) -> Vec<u8> {
    let mut plan = fixture["plan"].clone();
    plan["planDigest"] = fixture["planDigest"].clone();
    if let Some((key,value)) = altered { plan[key] = value; }
    let mut bytes = Vec::new();
    for key in ["operationId","archiveSha256","sourceGraphId","sourceUserId","targetGeneration","planDigest","expectedDocumentCount","expectedRdfTripleCount"] {
        let value = plan[key].as_str().map(str::to_string).unwrap_or_else(|| plan[key].to_string());
        bytes.extend_from_slice(format!("--restore-boundary\r\nContent-Disposition: form-data; name=\"{key}\"\r\n\r\n{value}\r\n").as_bytes());
    }
    bytes.extend_from_slice(b"--restore-boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"fixture.tar.gz\"\r\nContent-Type: application/gzip\r\n\r\n");
    bytes.extend(STANDARD.decode(fixture["archiveBase64"].as_str().unwrap()).unwrap());
    bytes.extend_from_slice(b"\r\n--restore-boundary--\r\n"); bytes
}

fn with_rdf(fixture: &Value, change: impl FnOnce(String) -> String) -> Value {
    with_archive_entries(fixture, |entries| {
        let (_, rdf) = entries.iter_mut().find(|(path, _)| path.ends_with(".nq")).unwrap();
        *rdf = change(String::from_utf8(rdf.clone()).unwrap()).into_bytes();
    })
}

fn with_archive_entries(fixture: &Value, change: impl FnOnce(&mut Vec<(String, Vec<u8>)>)) -> Value {
    use std::io::{Read, Write};
    let bytes = STANDARD.decode(fixture["archiveBase64"].as_str().unwrap()).unwrap();
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes.as_slice()));
    let mut entries = Vec::new();
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let path = entry.path().unwrap().to_string_lossy().into_owned();
        let mut data = Vec::new(); entry.read_to_end(&mut data).unwrap();
        entries.push((path,data));
    }
    change(&mut entries);
    let mut builder=tar::Builder::new(Vec::new());
    for (path,data) in entries {
        let mut header=tar::Header::new_ustar();header.set_size(data.len() as u64);header.set_mode(0o644);header.set_cksum();
        builder.append_data(&mut header,path,data.as_slice()).unwrap();
    }
    let mut gzip=flate2::write::GzEncoder::new(Vec::new(),flate2::Compression::default());
    gzip.write_all(&builder.into_inner().unwrap()).unwrap();let bytes=gzip.finish().unwrap();
    let mut changed=fixture.clone();changed["archiveBase64"]=json!(STANDARD.encode(&bytes));
    changed["plan"]["archiveSha256"]=json!(format!("{:x}",Sha256::digest(&bytes)));
    changed
}

fn with_source_title(fixture: &Value, title: &str) -> Value {
    use yrs::{Doc, Map, Out, ReadTxn, StateVector, Transact, Update};
    use yrs::updates::decoder::Decode;
    let mut changed = with_archive_entries(fixture, |entries| {
        let (_, workspace) = entries.iter_mut().find(|(path, _)| path == "crdt/workspace.yjs").unwrap();
        let doc = Doc::new();
        {
            let mut txn = doc.transact_mut();
            txn.apply_update(Update::decode_v1(workspace).unwrap()).unwrap();
            let documents = txn.get_map("documents").unwrap();
            let Out::YMap(document) = documents.get(&txn, "doc-fixture-alpha").unwrap() else { panic!("fixture document map") };
            document.insert(&mut txn, "title", title);
        }
        *workspace = doc.transact().encode_state_as_update_v1(&StateVector::default());
        entries.iter_mut().find(|(path, _)| path.ends_with(".nq")).unwrap().1.clear();
    });
    changed["plan"]["expectedRdfTripleCount"] = json!(0);
    changed
}

fn assert_unclaimed_empty(state: &LoopbackState, graph_dir: &std::path::Path) {
    assert!(!graph_dir.join(".migration/archive-restore-v1.json").exists());
    assert!(!crate::paths::workspace_ydoc_state_path(graph_dir).exists());
    assert!(!crate::paths::workspace_snapshot_path(graph_dir).exists());
    assert!(crate::document_service::list_documents(state.app.clone(), GRAPH.into()).unwrap().is_empty());
    assert!(crate::crdt_operation_journal::recover_pending_crdt_operations(&state.app).unwrap().is_empty());
}

async fn create_folder_normally(state: Arc<LoopbackState>, owner: &str, id: &str) {
    let (status, body) = request(state, &format!("/api/graphs/{GRAPH}/folders"), owner,
        serde_json::to_vec(&json!({"folderId":id,"name":"After restore admission","parentId":null,"section":"documents","order":99})).unwrap(), "application/json").await;
    assert!(status.is_success(), "normal folder mutation: {status} {body}");
}

async fn request(state: Arc<LoopbackState>, path: &str, token: &str, body: Vec<u8>, content_type: &str) -> (StatusCode, Value) {
    let response = crate::loopback_router::loopback_router(state).oneshot(Request::builder().method("POST").uri(path)
        .header("authorization",format!("Bearer {TOKEN}")).header("x-sophia-cell-lease",token)
        .header("content-type",content_type).body(Body::from(body)).unwrap()).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000).await.unwrap();
    (status,serde_json::from_slice(&bytes).unwrap_or_else(|_| json!({"raw":String::from_utf8_lossy(&bytes)})))
}

async fn await_job(state: &LoopbackState, body: &Value) -> crate::local_jobs::LocalJobRecord {
    await_job_for(state, body, 45).await
}

async fn await_job_for(state: &LoopbackState, body: &Value, seconds: u64) -> crate::local_jobs::LocalJobRecord {
    let id = body["job_id"].as_str().or_else(||body["jobId"].as_str()).expect("accepted response has job ID");
    tokio::time::timeout(std::time::Duration::from_secs(seconds), async {
        loop {
            let job = state.jobs.get(id).unwrap().unwrap();
            if matches!(job.status,LocalJobStatus::Succeeded|LocalJobStatus::Failed|LocalJobStatus::Cancelled) { return job; }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await.expect("bounded restore job")
}

#[test]
fn owned_restore_signed_http_queue_persistence_and_closed_negatives() {
    let _ = log::set_logger(&RESTORE_TEST_LOGGER);
    log::set_max_level(log::LevelFilter::Warn);
    let _serial = crate::tauri_runtime::profile_env_serial().lock().unwrap_or_else(|p|p.into_inner());
    let profile = std::env::temp_dir().join(format!("garden-owned-restore-{}",uuid::Uuid::new_v4()));
    std::env::set_var("GARDEN_PROFILE_DIR",&profile);
    let result = std::panic::catch_unwind(|| {
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap().block_on(async {
            let state = state(&profile);
            let graph = crate::graph_service::create_graph_service(&state.app,crate::graph_service::CreateGraphInput {
                graph_id:Some(GRAPH.into()),title:"Cross-format preservation fixture".into(),description:Some("Synthetic; no account or private content.".into()),operation_id:None,
            }).unwrap();
            let fixture: Value = serde_json::from_str(include_str!("../tests/fixtures/owned-restore-v1.json")).unwrap();
            let path = format!("/graphs/{GRAPH}/restore-archive");
            let mime = "multipart/form-data; boundary=restore-boundary";
            for role in ["viewer","editor"] {
                assert_eq!(request(state.clone(),&path,&lease(role,None),multipart(&fixture,None),mime).await.0,StatusCode::FORBIDDEN);
            }
            for changed in [("owner",json!("user:foreign")),("graphId",json!("foreign")),("generation",json!(2))] {
                assert_eq!(request(state.clone(),&path,&lease("owner",Some(changed)),multipart(&fixture,None),mime).await.0,StatusCode::UNAUTHORIZED);
            }
            let owner = lease("owner",None);
            for (key,value,expected) in [("targetGeneration",json!(2),StatusCode::CONFLICT),("sourceUserId",json!("foreign"),StatusCode::FORBIDDEN),
                ("sourceGraphId",json!("foreign"),StatusCode::CONFLICT),("archiveSha256",json!("0".repeat(64)),StatusCode::BAD_REQUEST),("planDigest",json!("bad-plan"),StatusCode::BAD_REQUEST)] {
                assert_eq!(request(state.clone(),&path,&owner,multipart(&fixture,Some((key,value))),mime).await.0,expected,"{key}");
            }
            assert!(!request(state.clone(),"/graphs/import",&owner,multipart(&fixture,None),mime).await.0.is_success());
            for kind in ["graph.importArchive","graph.restoreArchive"] {
                let rpc=json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"crdt_operation","arguments":{"kind":kind,"graph_id":GRAPH,"payload":{"newGraphId":GRAPH}}}});
                let (_,body)=request(state.clone(),"/mcp",&owner,serde_json::to_vec(&rpc).unwrap(),"application/json").await;
                assert!(body.get("error").is_some() || body["result"]["isError"]==true,"MCP bypass must fail: {body}");
            }
            for key in ["expectedDocumentCount","expectedRdfTripleCount"] {
                let (status,body)=request(state.clone(),&path,&owner,multipart(&fixture,Some((key,json!(99)))),mime).await;
                assert_eq!(status,StatusCode::ACCEPTED,"{body}");
                assert!(matches!(await_job(&state,&body).await.status,LocalJobStatus::Failed));
            }
            let graph_dir=crate::graph_paths::existing_graph_dir(&state.app,GRAPH).unwrap();
            let source_graph=format!("urn:mnemosyne:user:fixture-cloud1-owner-20260908:graph:{GRAPH}");
            let invalid_fixtures=[
                with_rdf(&fixture, |_| "this is invalid RDF".into()),
                with_rdf(&fixture, |rdf|rdf.replace(&format!("<{source_graph}> ."),&format!("<urn:mnemosyne:local:graph:{GRAPH}:projection:workspace> ."))),
                with_rdf(&fixture, |rdf|rdf.replace("\"An ordered garden\"", "\"Conflicting source title\"")),
            ];
            for invalid in invalid_fixtures {
                let (status,body)=request(state.clone(),&path,&owner,multipart(&invalid,None),mime).await;
                assert_eq!(status,StatusCode::ACCEPTED,"{body}");
                let failed=await_job(&state,&body).await;
                assert!(matches!(failed.status,LocalJobStatus::Failed),"{failed:?}");
                assert!(!graph_dir.join(".migration/archive-restore-v1.json").exists());
                assert!(!crate::paths::workspace_ydoc_state_path(&graph_dir).exists());
                assert!(crate::document_service::list_documents(state.app.clone(),GRAPH.into()).unwrap().is_empty());
                assert!(crate::crdt_operation_journal::recover_pending_crdt_operations(&state.app).unwrap().is_empty(),"invalid input must be terminal, without retry");
            }
            // Imported stored titles use the bounded preservation policy, not
            // the public create-dialog's 96-character input policy.
            for title in ["x".repeat(65537), "   ".to_string(), " padded historical title ".to_string()] {
                let invalid = with_source_title(&fixture, &title);
                let (status, body) = request(state.clone(), &path, &owner, multipart(&invalid, None), mime).await;
                assert_eq!(status, StatusCode::ACCEPTED, "{body}");
                let failed = await_job(&state, &body).await;
                assert!(matches!(failed.status, LocalJobStatus::Failed), "{failed:?}");
                assert!(failed.error.as_deref().unwrap_or("").contains("unsupported source document title"), "{failed:?}");
                assert_unclaimed_empty(&state, &graph_dir);
            }
            // Real deletion fences are authority even for IDs outside this archive.
            for id in ["doc-fixture-alpha", "deleted-id-outside-archive"] {
                let tombstone = crate::document_tombstone_store::write_document_tombstone_for_operation(&graph_dir, id, Some("prior-deletion")).unwrap();
                let tombstone_path = crate::document_tombstone_store::document_tombstone_path(&graph_dir, id).unwrap();
                let before = std::fs::read(&tombstone_path).unwrap();
                let (status, body) = request(state.clone(), &path, &owner, multipart(&fixture, None), mime).await;
                assert_eq!(status, StatusCode::ACCEPTED, "{body}");
                let failed = await_job(&state, &body).await;
                assert!(matches!(failed.status, LocalJobStatus::Failed), "{failed:?}");
                assert!(failed.error.as_deref().unwrap_or("").contains("stored content"), "{failed:?}");
                assert_unclaimed_empty(&state, &graph_dir);
                assert_eq!(std::fs::read(&tombstone_path).unwrap(), before);
                assert_eq!(crate::document_tombstone_store::read_document_tombstone(&graph_dir, id).unwrap(), Some(tombstone));
                // Remove only this test-created fixture fence, never in product admission.
                std::fs::remove_file(tombstone_path).unwrap();
            }
            // A real non-user named graph must not escape the empty-target gate.
            use oxigraph::model::{NamedNode, Quad};
            let store=crate::rdf_store_service::open_graph_store(&graph_dir).unwrap();
            let unrelated=Quad::new(NamedNode::new("urn:prior:s").unwrap(),NamedNode::new("urn:prior:p").unwrap(),NamedNode::new("urn:prior:o").unwrap(),NamedNode::new("urn:prior:graph").unwrap());
            store.insert(&unrelated).unwrap();
            let (_,body)=request(state.clone(),&path,&owner,multipart(&fixture,None),mime).await;
            let failed=await_job(&state,&body).await;
            assert!(failed.error.as_deref().unwrap_or("").contains("already has RDF"),"{failed:?}");
            assert!(store.contains(&unrelated).unwrap());assert!(!crate::paths::workspace_ydoc_state_path(&graph_dir).exists());
            store.remove(&unrelated).unwrap();drop(store);
            let orphan=crate::paths::document_ydoc_state_path(&graph_dir,"orphan-absent-from-records");
            crate::storage::create_dir_all(orphan.parent().unwrap()).unwrap();
            crate::storage::write_bytes(&orphan,b"orphan authority").unwrap();
            let (_,body)=request(state.clone(),&path,&owner,multipart(&fixture,None),mime).await;
            let failed=await_job(&state,&body).await;
            assert!(failed.error.as_deref().unwrap_or("").contains("stored content"),"{failed:?}");
            assert_eq!(std::fs::read(&orphan).unwrap(),b"orphan authority");
            std::fs::remove_file(&orphan).unwrap();std::fs::remove_dir(orphan.parent().unwrap()).unwrap();
            let (status,body)=request(state.clone(),&path,&owner,multipart(&fixture,None),mime).await;
            assert_eq!(status,StatusCode::ACCEPTED,"{body}");
            let completed=await_job(&state,&body).await;
            assert!(matches!(completed.status,LocalJobStatus::Succeeded),"{completed:?}");
            assert_eq!(completed.owner_principal.as_deref(),Some(OWNER));
            assert_eq!(completed.graph_generation,Some(1));
            let (_,after)=crate::graph_service::read_graph_record(&state.app,GRAPH).unwrap();
            assert_eq!(graph.incarnation_id,after.incarnation_id);
            assert_eq!(graph.created_at,after.created_at);
            let mut documents=serde_json::Map::new();
            for id in ["doc-fixture-alpha","doc-fixture-beta"] {
                let document=crate::document_service::read_document(state.app.clone(),GRAPH.into(),id.into()).unwrap();
                assert_eq!(document.revision,1);
                let graph_dir=crate::graph_paths::existing_graph_dir(&state.app,GRAPH).unwrap();
                let bytes=std::fs::read(crate::paths::document_ydoc_state_path(&graph_dir,id)).unwrap();
                documents.insert(id.into(),json!(STANDARD.encode(bytes)));
            }
            let graph_dir=crate::graph_paths::existing_graph_dir(&state.app,GRAPH).unwrap();
            let workspace=std::fs::read(crate::paths::workspace_ydoc_state_path(&graph_dir)).unwrap();
            let rdf=crate::rdf_service::dump_rdf_service(state.app.clone(),crate::rdf_service::RdfDumpInput {
                graph_id:GRAPH.into(),format:"application/n-quads".into(),source_graph_iri:None,
            }).unwrap();
            let user_graph=crate::rdf_authority::user_rdf_graph_iri(GRAPH);
            let authored=crate::rdf_service::dump_rdf_service(state.app.clone(),crate::rdf_service::RdfDumpInput {
                graph_id:GRAPH.into(),format:"application/n-quads".into(),source_graph_iri:Some(user_graph.clone()),
            }).unwrap();
            let user_quads=oxigraph::io::RdfParser::from_format(oxigraph::io::RdfFormat::NQuads).for_slice(authored.data.as_bytes()).collect::<Result<Vec<_>,_>>().unwrap();
            assert_eq!(user_quads.len(),4);
            let query=format!("ASK {{ GRAPH <{user_graph}> {{ <urn:mnemosyne:local:document:doc-fixture-alpha> <urn:demi:fixture:annotation> ?note . ?note <urn:demi:fixture:text> \"moss is not fern\"@en }} }}");
            let answer=crate::rdf_service::run_sparql_query_service(state.app.clone(),crate::rdf_service::SparqlInput { graph_id:GRAPH.into(),query }).unwrap();
            assert_eq!(answer.boolean,Some(true));
            if let Ok(output)=std::env::var("GARDEN_OWNED_RESTORE_OBSERVATION") {
                let observation=json!({"identity":{"sourceUserId":fixture["plan"]["sourceUserId"],"graphId":GRAPH,"ownerPrincipal":OWNER},
                    "graphMetadata":{"title":after.title,"description":after.description},"workspaceYjsBase64":STANDARD.encode(workspace),
                    "documentYjsBase64":documents,"mainRdfNquads":rdf.data,"userRdfNquads":authored.data,
                    "rdfPolicy":"cloud1-main-canonical-v2",
                    "evidence":{"caller":"signed Axum HTTP -> durable job -> real queue","fixtureArchiveSha256":fixture["plan"]["archiveSha256"],"profile":profile}});
                crate::storage::write_bytes(std::path::Path::new(&output),&serde_json::to_vec_pretty(&observation).unwrap()).unwrap();
            }
            let (_,body)=request(state.clone(),&path,&owner,multipart(&fixture,None),mime).await;
            assert!(matches!(await_job(&state,&body).await.status,LocalJobStatus::Succeeded));
            let (_,body)=request(state.clone(),&path,&owner,multipart(&fixture,Some(("planDigest",json!("f".repeat(64))))),mime).await;
            assert!(matches!(await_job(&state,&body).await.status,LocalJobStatus::Failed));
            let (_,body)=request(state.clone(),&path,&owner,multipart(&fixture,Some(("operationId",json!("different-operation")))),mime).await;
            assert!(matches!(await_job(&state,&body).await.status,LocalJobStatus::Failed));
            create_folder_normally(state.clone(), &owner, "folder-after-successful-restore").await;
            let registry = state.app.state::<crate::crdt_engine::rooms::RoomRegistry>();
            let room = registry.existing_room(&format!("workspace:{GRAPH}")).unwrap().unwrap();
            let live = room.with_doc(|doc| crate::crdt_engine::workspace_ops::materialize_workspace_snapshot_json(GRAPH, doc).unwrap()).await;
            let cold: Value = crate::storage::read_json(&crate::paths::workspace_snapshot_path(&graph_dir)).unwrap();
            assert_eq!(live["documents"], cold["documents"]);
            assert_eq!(live["documents"].as_array().unwrap().len(), 2);
            assert!(cold["folders"].as_array().unwrap().iter().any(|folder| folder["id"] == "folder-after-successful-restore"));
        });
    });
    std::env::remove_var("GARDEN_PROFILE_DIR");
    let _=std::fs::remove_dir_all(&profile);
    if let Err(error)=result { std::panic::resume_unwind(error); }
}

#[test]
fn owned_restore_orphan_text_receipts_refuse_before_publication() {
    let _serial = crate::tauri_runtime::profile_env_serial().lock().unwrap_or_else(|p| p.into_inner());
    // Receipt contents and operation IDs do not confer permission to discard
    // authority. An empty directory, in contrast, contains no receipt authority.
    for (case, receipt) in [
        ("malformed", Some(b"{ retained malformed receipt\n".as_slice())),
        ("unrelated", Some(br#"{"operationId":"unrelated-prior-operation","artifactId":"absent-artifact","graphIncarnation":"prior-incarnation"}"#.as_slice())),
        ("empty-directory", None),
    ] {
        let profile = std::env::temp_dir().join(format!("garden-owned-restore-text-{case}-{}", uuid::Uuid::new_v4()));
        std::env::set_var("GARDEN_PROFILE_DIR", &profile);
        let result = std::panic::catch_unwind(|| {
            tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap().block_on(async {
                let state = state(&profile);
                let graph = crate::graph_service::create_graph_service(&state.app, crate::graph_service::CreateGraphInput {
                    graph_id: Some(GRAPH.into()), title: "Receipt authority fixture".into(), description: None, operation_id: None,
                }).unwrap();
                let graph_dir = crate::graph_paths::existing_graph_dir(&state.app, GRAPH).unwrap();
                let receipt_dir = graph_dir.join("artifact-text-operations");
                let receipt_path = receipt_dir.join("unrelated-operation.json");
                crate::storage::create_dir_all(&receipt_dir).unwrap();
                if let Some(bytes) = receipt { crate::storage::write_bytes(&receipt_path, bytes).unwrap(); }
                assert_unclaimed_empty(&state, &graph_dir);
                assert!(std::fs::read_dir(crate::paths::artifacts_dir(&graph_dir))
                    .map(|mut entries| entries.next().is_none())
                    .unwrap_or_else(|error| error.kind() == std::io::ErrorKind::NotFound),
                    "{case}: fixture must contain no artifact authority");
                let store = crate::rdf_store_service::open_graph_store(&graph_dir).unwrap();
                let before_rdf = store.iter().map(|q| q.unwrap().to_string()).collect::<std::collections::BTreeSet<_>>();
                let fixture: Value = serde_json::from_str(include_str!("../tests/fixtures/owned-restore-v1.json")).unwrap();
                let owner = lease("owner", None);
                let (status, body) = request(state.clone(), &format!("/graphs/{GRAPH}/restore-archive"), &owner,
                    multipart(&fixture, None), "multipart/form-data; boundary=restore-boundary").await;
                assert_eq!(status, StatusCode::ACCEPTED, "{case}: {body}");
                let job = await_job(&state, &body).await;
                let retained = std::fs::read(&receipt_path).ok();
                let after_rdf = store.iter().map(|q| q.unwrap().to_string()).collect::<std::collections::BTreeSet<_>>();
                println!("GARDEN_ORPHAN_TEXT_RECEIPT_WITNESS {}", json!({
                    "case": case, "jobStatus": format!("{:?}", job.status), "error": job.error,
                    "receiptBeforeBase64": receipt.map(|bytes| STANDARD.encode(bytes)),
                    "receiptAfterBase64": retained.as_ref().map(|bytes| STANDARD.encode(bytes)),
                    "restoreMarkerExists": graph_dir.join(".migration/archive-restore-v1.json").exists(),
                    "workspaceExists": crate::paths::workspace_ydoc_state_path(&graph_dir).exists(),
                    "rdfUnchanged": before_rdf == after_rdf,
                }));
                if let Some(bytes) = receipt {
                    assert!(matches!(job.status, LocalJobStatus::Failed), "{case}: orphan receipt must refuse before publication: {job:?}");
                    assert!(job.error.as_deref().unwrap_or("").contains("stored content"), "{case}: {job:?}");
                    assert_eq!(retained.as_deref(), Some(bytes), "{case}: receipt bytes changed");
                    assert_eq!(std::fs::read_dir(&receipt_dir).unwrap().count(), 1);
                    assert_unclaimed_empty(&state, &graph_dir);
                    assert_eq!(before_rdf, after_rdf, "{case}: RDF changed before refusal");
                    assert!(std::fs::read_dir(crate::paths::artifacts_dir(&graph_dir))
                        .map(|mut entries| entries.next().is_none())
                        .unwrap_or_else(|error| error.kind() == std::io::ErrorKind::NotFound),
                        "{case}: refused restore must publish no artifact authority");
                    let (_, after_graph) = crate::graph_record_store::read_graph_record_no_heal(&state.app, GRAPH).unwrap();
                    assert_eq!(serde_json::to_value(&graph).unwrap(), serde_json::to_value(&after_graph).unwrap());
                } else {
                    assert!(matches!(job.status, LocalJobStatus::Succeeded), "empty receipt directory must remain admissible: {job:?}");
                    assert_eq!(crate::document_service::list_documents(state.app.clone(), GRAPH.into()).unwrap().len(), 2);
                    assert!(graph_dir.join(".migration/archive-restore-v1.json").is_file());
                    assert_eq!(std::fs::read_dir(&receipt_dir).unwrap().count(), 0);
                    let (_, replay) = request(state.clone(), &format!("/graphs/{GRAPH}/restore-archive"), &owner,
                        multipart(&fixture, None), "multipart/form-data; boundary=restore-boundary").await;
                    assert!(matches!(await_job(&state, &replay).await.status, LocalJobStatus::Succeeded));
                }
            });
        });
        std::env::remove_var("GARDEN_PROFILE_DIR");
        // Remove only this synthetic, uniquely named fixture profile; witness
        // bytes remain encoded in the retained test transcript on red and green.
        let _ = std::fs::remove_dir_all(&profile);
        if let Err(error) = result { std::panic::resume_unwind(error); }
    }
}

#[test]
fn owned_restore_preopened_room_refuses_without_eviction_and_normal_mutation_survives() {
    let _serial = crate::tauri_runtime::profile_env_serial().lock().unwrap_or_else(|p| p.into_inner());
    let profile = std::env::temp_dir().join(format!("garden-owned-restore-room-{}", uuid::Uuid::new_v4()));
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let result = std::panic::catch_unwind(|| {
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap().block_on(async {
            let state = state(&profile);
            crate::graph_service::create_graph_service(&state.app, crate::graph_service::CreateGraphInput {
                graph_id: Some(GRAPH.into()), title: "Preopened target".into(), description: None, operation_id: None,
            }).unwrap();
            let graph_dir = crate::graph_paths::existing_graph_dir(&state.app, GRAPH).unwrap();
            let registry = state.app.state::<crate::crdt_engine::rooms::RoomRegistry>();
            let key = format!("workspace:{GRAPH}");
            let room = registry.get_or_create(&key, crate::paths::workspace_ydoc_state_path(&graph_dir)).await.unwrap();
            let fixture: Value = serde_json::from_str(include_str!("../tests/fixtures/owned-restore-v1.json")).unwrap();
            let owner = lease("owner", None);
            let (status, body) = request(state.clone(), &format!("/graphs/{GRAPH}/restore-archive"), &owner,
                multipart(&fixture, None), "multipart/form-data; boundary=restore-boundary").await;
            assert_eq!(status, StatusCode::ACCEPTED, "{body}");
            let failed = await_job(&state, &body).await;
            assert!(matches!(failed.status, LocalJobStatus::Failed), "{failed:?}");
            assert!(failed.error.as_deref().unwrap_or("").contains("live room authority"), "{failed:?}");
            assert_unclaimed_empty(&state, &graph_dir);
            assert!(Arc::ptr_eq(&room, &registry.existing_room(&key).unwrap().unwrap()));
            create_folder_normally(state.clone(), &owner, "folder-after-refusal").await;
            assert!(Arc::ptr_eq(&room, &registry.existing_room(&key).unwrap().unwrap()));
            let live = room.with_doc(|doc| crate::crdt_engine::workspace_ops::materialize_workspace_snapshot_json(GRAPH, doc).unwrap()).await;
            let cold: Value = crate::storage::read_json(&crate::paths::workspace_snapshot_path(&graph_dir)).unwrap();
            assert_eq!(live["folders"], cold["folders"]);
            assert_eq!(cold["documents"].as_array().unwrap().len(), 0);
            assert!(cold["folders"].as_array().unwrap().iter().any(|folder| folder["id"] == "folder-after-refusal"));
        });
    });
    std::env::remove_var("GARDEN_PROFILE_DIR");
    let _ = std::fs::remove_dir_all(&profile);
    if let Err(error) = result { std::panic::resume_unwind(error); }
}
#[path = "owned_preservation_v2_examiner_tests.rs"]
mod preservation_v2_examiner;
