//! Actual public MCP handler/source-activated integration over a disposable graph.
//! Source/document reference fields are synthetic custody specimens, not resolved
//! documents or certified book quotations. No HTTP authentication or hosted claim.
use crate::app_runtime::AppHandle;
use crate::emporium::{
    contract::{get_vocabulary, Datatype, SourceKind},
    planner::plan_generic_compute,
    schemas::GenericRecordIn,
    shacl_validator::validate_desired,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

const PACK: &str = "ludus-core";
const GRAPH: &str = "ludus-native";
const NS: &str = "https://sophia-labs.com/ns/ludus#";
fn fixtures() -> Vec<Value> {
    serde_json::from_str(include_str!("ludus_source_fixture.json")).unwrap()
}
fn subject(kind: &str, id: &str) -> String {
    let slug = match kind {
        "SourceEdition" => "source-edition",
        "SourceOccurrence" => "source-occurrence",
        "Concept" => "concept",
        "Encounter" => "encounter",
        "AttemptSubmitted" => "attempt",
        "FeedbackRecorded" => "feedback",
        _ => panic!("unknown class"),
    };
    format!("urn:mnemosyne:local:graph:{GRAPH}:projection:ludus:{slug}:{id}")
}
fn checked(value: &Value) -> bool {
    let input: GenericRecordIn = match serde_json::from_value(value.clone()) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let c = get_vocabulary(PACK).unwrap();
    match plan_generic_compute(c, GRAPH, &[input]) {
        Ok(p) => validate_desired(&p.desired_inserts, c).is_ok(),
        Err(_) => false,
    }
}
#[test]
fn ludus_source_registered_six_class_contract_and_all_required_fields() {
    assert_eq!(
        format!(
            "{:x}",
            Sha256::digest(crate::emporium::vocabs::LUDUS_CORE_GOLDEN_JSON.as_bytes())
        ),
        crate::emporium::vocabs::LUDUS_CORE_GOLDEN_SHA
    );
    let c = get_vocabulary(PACK).unwrap();
    assert_eq!(c.classes.len(), 6);
    assert_eq!(c.version, "1.0.0");
    for record in fixtures() {
        assert!(checked(&record), "valid fixture: {record}");
        let class = record["kind"].as_str().unwrap();
        let sig = c.materialization_signature(class).unwrap();
        assert_eq!(
            sig.source_kind,
            if ["AttemptSubmitted", "FeedbackRecorded"].contains(&class) {
                SourceKind::EventLog
            } else {
                SourceKind::CurrentState
            }
        );
        assert_eq!(sig.store_target, "projection:ludus");
        for (predicate, spec) in &c.classes[class].predicates {
            if spec.required {
                let mut bad = record.clone();
                bad.as_object_mut()
                    .unwrap()
                    .remove(predicate.strip_prefix("ludus:").unwrap());
                assert!(!checked(&bad), "missing {class}.{predicate}");
            }
        }
        let mut unknown = record.clone();
        unknown["unknownField"] = json!("must refuse");
        assert!(!checked(&unknown));
        let mut unsafe_id = record.clone();
        unsafe_id["localId"] = json!("bad > subject");
        assert!(!checked(&unsafe_id));
    }
    let mut bad = fixtures()[0].clone();
    bad["sourceSha256"] = json!("not-a-hash");
    assert!(!checked(&bad));
    let mut bad = fixtures()[1].clone();
    bad["startByte"] = json!(-1);
    assert!(!checked(&bad));
    let mut bad = fixtures()[1].clone();
    bad["startByte"] = json!(72850);
    assert!(!checked(&bad));
}

fn assert_read_matches(input: &Value, actual: &Value) {
    let c = get_vocabulary(PACK).unwrap();
    let class = input["kind"].as_str().unwrap();
    assert_eq!(
        actual["subject"],
        subject(class, input["localId"].as_str().unwrap())
    );
    for (name, value) in input.as_object().unwrap() {
        if name == "kind" || name == "localId" {
            continue;
        }
        let datatype = c.classes[class].predicates[&format!("ludus:{name}")].datatype;
        let values = value
            .as_array()
            .cloned()
            .unwrap_or_else(|| vec![value.clone()]);
        let expected: BTreeSet<String> = values
            .iter()
            .map(|v| match datatype {
                Datatype::uri => format!("<{}>", v.as_str().unwrap()),
                Datatype::integer => oxigraph::model::Literal::new_typed_literal(
                    v.as_i64().unwrap().to_string(),
                    oxigraph::model::NamedNode::new_unchecked(
                        "http://www.w3.org/2001/XMLSchema#integer",
                    ),
                )
                .to_string(),
                Datatype::dateTime => oxigraph::model::Literal::new_typed_literal(
                    v.as_str().unwrap(),
                    oxigraph::model::NamedNode::new_unchecked(
                        "http://www.w3.org/2001/XMLSchema#dateTime",
                    ),
                )
                .to_string(),
                Datatype::string => {
                    oxigraph::model::Literal::new_simple_literal(v.as_str().unwrap()).to_string()
                }
                _ => panic!("unexpected fixture datatype"),
            })
            .collect();
        let found: BTreeSet<String> = actual["predicates"][format!("{NS}{name}")]
            .as_array()
            .unwrap_or_else(|| panic!("missing {class}.{name}: {actual}"))
            .iter()
            .map(|v| v.as_str().unwrap().to_owned())
            .collect();
        assert_eq!(found, expected, "actual field {class}.{name}");
    }
}
async fn write(
    app: &AppHandle,
    record: &Value,
    operation: &str,
) -> crate::app_error::AppResult<Value> {
    crate::emporium_mcp_surface::mcp_local_emporium_write(app.clone(),&json!({"graphId":GRAPH,"vocab":PACK,"sourceOperationId":operation,"atMs":1788933600000_i64,"records":[record]})).await
}
async fn pull(app: &AppHandle) -> Value {
    crate::source_sync::mcp_local_source_pull(app.clone(), &json!({"graphId":GRAPH}))
        .await
        .unwrap()
}
fn read(app: &AppHandle, record: &Value) -> Value {
    crate::emporium_mcp_surface::mcp_local_emporium_read(
        app.clone(),
        &json!({"graphId":GRAPH,"vocab":PACK,"class":record["kind"],"address":record["localId"]}),
    )
    .unwrap()
}

#[test]
fn ludus_source_public_activated_write_query_events_replay_and_rebuild() {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var_os("GARDEN_PROFILE_DIR");
    let profile =
        std::env::temp_dir().join(format!("garden-ludus-native-{}", uuid::Uuid::new_v4()));
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    println!("LUDUS_DISPOSABLE_PROFILE={}", profile.display());
    let outcome = std::panic::catch_unwind(|| {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        crate::graph_service::create_graph_service(
            &app,
            crate::graph_service::CreateGraphInput {
                graph_id: Some(GRAPH.into()),
                title: "Synthetic Ludus source fixture".into(),
                description: None,
                operation_id: None,
            },
        )
        .unwrap();
        crate::app_runtime::async_runtime::block_on(async {
            assert!(!crate::source_sync::source_authority_active(&app, GRAPH).unwrap());
            let graph_dir = crate::paths::existing_graph_dir(&app, GRAPH).unwrap();
            let store = crate::rdf_service::open_graph_store(&graph_dir).unwrap();
            let before_activation =
                crate::rdf_query_service::dump_rdf_from_store(&store, "nquads", None, None)
                    .unwrap()
                    .data;
            for record in fixtures() {
                let refused = write(&app, &record, "inactive-must-refuse")
                    .await
                    .unwrap_err();
                assert!(refused
                    .to_string()
                    .contains("require active source authority"));
            }
            let preview = crate::emporium_mcp_surface::mcp_local_emporium_write(
                app.clone(),
                &json!({"graphId":GRAPH,"vocab":PACK,"dryRun":true,"records":[fixtures()[4]]}),
            )
            .await
            .unwrap();
            assert_eq!(
                preview["ok"], true,
                "inactive validation preview: {preview}"
            );
            assert!(!crate::source_sync::source_authority_active(&app, GRAPH).unwrap());
            assert_eq!(
                before_activation,
                crate::rdf_query_service::dump_rdf_from_store(&store, "nquads", None, None)
                    .unwrap()
                    .data,
                "inactive refusal and preview must not mutate any projection",
            );
            drop(store);
            let initial = pull(&app).await;
            assert_eq!(initial["complete"], true);
            let registry: Vec<_> = initial["sourceRegistry"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|x| x["vocab"] == PACK)
                .collect();
            assert_eq!(registry.len(), 6);
            let records = fixtures();
            for (i, record) in records.iter().enumerate() {
                let receipt = write(&app, record, &format!("ludus-op-{i}")).await.unwrap();
                assert_eq!(receipt["ok"], true, "{receipt}");
                assert_eq!(
                    receipt["sourceSync"]["ok"], true,
                    "public source route: {receipt}"
                );
                assert_read_matches(record, &read(&app, record));
            }
            let a = &records[4];
            let replay = write(&app, a, "ludus-op-4").await.unwrap();
            assert_eq!(
                replay["sourceSync"]["receipts"][0]["duplicate"], true,
                "{replay}"
            );
            let same_event = write(&app, a, "ludus-op-4-other-transport").await.unwrap();
            assert_eq!(same_event["ok"], true);
            let before = pull(&app).await;
            let mut changed = a.clone();
            changed["answer"] = json!("changed payload must refuse");
            assert!(write(&app, &changed, "ludus-op-4").await.is_err());
            assert!(write(&app, &changed, "ludus-event-identity-reuse")
                .await
                .is_err());
            let after = pull(&app).await;
            assert_eq!(before["events"], after["events"]);
            assert_eq!(before["revision"], after["revision"]);
            assert_eq!(before["projectionSnapshot"], after["projectionSnapshot"]);
            let mut revision = a.clone();
            revision["localId"] = json!("attempt-b");
            revision["answer"] = json!("The lady loves the daughter.");
            revision["reasoning"] =
                json!("The endings reverse my earlier position-based reasoning.");
            revision["revises"] = json!(subject("AttemptSubmitted", "attempt-a"));
            revision["assistanceJson"] = json!(format!(
                "[\"{}\"]",
                subject("FeedbackRecorded", "feedback-a")
            ));
            assert_eq!(
                write(&app, &revision, "ludus-revision-b").await.unwrap()["ok"],
                true
            );
            assert_read_matches(a, &read(&app, a));
            assert_read_matches(&revision, &read(&app, &revision));
            let query=crate::emporium_mcp_surface::mcp_local_emporium_query(app.clone(),&json!({"graphId":GRAPH,"vocab":PACK,"class":"AttemptSubmitted","criteria":{"learnerId":"synthetic-test-user-not-Vera"}})).unwrap();
            assert_eq!(query["totalMatched"], 2, "{query}");
            let ledger = pull(&app).await;
            assert_eq!(
                ledger["currentState"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|x| x["vocab"] == PACK)
                    .count(),
                4
            );
            assert_eq!(
                ledger["events"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|x| x["vocab"] == PACK)
                    .count(),
                3
            );
            let direct=crate::source_sync::mcp_local_source_push(app.clone(),&json!({"graphId":GRAPH,"graphIncarnation":ledger["graphIncarnation"],"operations":[{"kind":"eventLog","operationId":"wrong-event-local-id","eventId":"event-other","vocab":PACK,"class":"AttemptSubmitted","record":a}]})).await;
            assert!(direct.is_err() || direct.as_ref().is_ok_and(|v| v["ok"] == false));
            let rebuilt = crate::source_sync::mcp_local_source_rebuild(
                app.clone(),
                &json!({"graphId":GRAPH,"graphIncarnation":ledger["graphIncarnation"]}),
            )
            .await
            .unwrap();
            assert_eq!(rebuilt["ok"], true, "{rebuilt}");
            for record in records.iter().chain(std::iter::once(&revision)) {
                assert_read_matches(record, &read(&app, record));
            }
            let final_pull = pull(&app).await;
            assert_eq!(final_pull["events"], ledger["events"]);
            if let Ok(file) = std::env::var("LUDUS_NATIVE_OBSERVATION") {
                std::fs::write(file,serde_json::to_vec_pretty(&json!({"evidenceKind":"actual-in-process-public-handlers-disposable-source-fixture","profile":profile,"sourcePull":final_pull,"query":query,"rebuild":rebuilt,"submittedRecords":records,"revision":revision})).unwrap()).unwrap();
            }
        });
    });
    if let Some(value) = previous {
        std::env::set_var("GARDEN_PROFILE_DIR", value);
    } else {
        std::env::remove_var("GARDEN_PROFILE_DIR");
    }
    // The unique fixture remains inspectable; no broad recursive cleanup.
    if let Err(error) = outcome {
        std::panic::resume_unwind(error);
    }
}


/// Prepared regression: real public handlers and exact full source response.
/// Resetting a test cache is not an independent process/OS recovery witness.
#[test]
fn ludus_source_checkpoint_replay_survives_seed_cache_reset() {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var_os("GARDEN_PROFILE_DIR");
    let profile = std::env::temp_dir()
        .join(format!("garden-seed-replay-{}", uuid::Uuid::new_v4()));
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    println!("SEED_REPLAY_DISPOSABLE_PROFILE={}", profile.display());
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        crate::graph_service::create_graph_service(
            &app,
            crate::graph_service::CreateGraphInput {
                graph_id: Some(GRAPH.into()),
                title: "Seed replay coherence specimen".into(),
                description: None,
                operation_id: None,
            },
        ).unwrap();
        crate::app_runtime::async_runtime::block_on(async {
            let initial = pull(&app).await;
            let graph_dir = crate::paths::existing_graph_dir(&app, GRAPH).unwrap();
            let store_path = graph_dir.join("store.oxigraph");
            let (_, initial_record) =
                crate::graph_record_store::read_graph_record_no_heal(&app, GRAPH).unwrap();
            let document = crate::document_create_once_mcp::create_document_once(
                app.clone(),
                &json!({
                    "graph_id": GRAPH, "graphIncarnation": initial["graphIncarnation"],
                    "document_id": "seed-page", "title": "Seed coherence page",
                    "order": 10, "parentId": null, "awaitDurable": false,
                    "tiptapJson": {"type":"doc","content":[{
                        "type":"paragraph","attrs":{"data-block-id":"seed-page-block"},
                        "content":[{"type":"text","text":"Retained canonical source content."}]
                    }]}
                }),
            ).await.unwrap();
            assert_eq!(document["outcome"], "created");
            let (_, current_record) =
                crate::graph_record_store::read_graph_record_no_heal(&app, GRAPH).unwrap();
            assert_ne!(initial_record.content_revision, current_record.content_revision,
                "the fixture must advance content beyond its initial checkpoint");
            let mut record = fixtures().into_iter().find(|r| r["kind"] == "Concept").unwrap();
            record["localId"] = json!("seed-concept");
            record["documentId"] = json!("seed-page");
            record["explanation"] = json!("First line\r\nSecond line — exact retained text.");
            let source_write_walks_before = crate::rdf_seed_service::reseed_count(&store_path);
            let applied = crate::emporium_mcp_surface::mcp_local_emporium_write(
                app.clone(),
                &json!({"graph_id":GRAPH,"graphIncarnation":initial["graphIncarnation"],
                    "vocab":PACK,"operationId":"seed-concept-operation",
                    "atMs":1788946452000_i64,"records":[record.clone()]}),
            ).await.unwrap();
            assert_eq!(applied["sourceSync"]["receipts"][0]["status"], "applied");
            assert_read_matches(&record, &read(&app, &record));
            let before = pull(&app).await;
            let assert_metadata = |bundle: &Value| {
                let canonical = crate::document_record_store::read_document_record_cold(
                    &graph_dir, &graph_dir.join("documents/seed-page/document.json"),
                ).unwrap();
                assert!(!canonical.body.is_empty(), "nonempty real content oracle");
                assert!(!canonical.tiptap_xml.is_empty(), "nonempty canonical XML oracle");
                let simple = |v: &str| oxigraph::model::Literal::new_simple_literal(v).to_string();
                let integer = |v: u64| format!("\"{v}\"^^<http://www.w3.org/2001/XMLSchema#integer>");
                let expected = std::collections::BTreeMap::from([
                    ("body", simple(&canonical.body)),
                    ("documentId", simple(&canonical.document_id)),
                    ("graphId", simple(&canonical.graph_id)),
                    ("localPath", simple(&canonical.local_path)),
                    ("origin", simple(&canonical.origin)),
                    ("providerId", simple(&canonical.provider_id)),
                    ("rdfTripleCount", integer(crate::rdf_document_tree::document_tree_triples(&canonical).len() as u64)),
                    ("schemaVersion", integer(canonical.schema_version as u64)),
                    ("tiptapXml", simple(&canonical.tiptap_xml)),
                    ("ydocStatePath", simple(&canonical.ydoc_state_path)),
                ]);
                let prefix = format!("<{}> <{}", canonical.rdf_subject, crate::runtime_config::MNEMO_NS);
                let suffix = format!(" <{}> .", crate::rdf_authority::document_projection_graph_iri(GRAPH, "seed-page"));
                let actual_rows = bundle["projectionSnapshot"]["data"].as_str().unwrap().lines()
                    .filter_map(|line| line.strip_prefix(&prefix))
                    .filter_map(|line| line.split_once("> "))
                    .filter(|(predicate, _)| expected.contains_key(predicate))
                    .map(|(predicate, object)| (predicate, object.strip_suffix(&suffix).unwrap().to_string()))
                    .collect::<Vec<_>>();
                assert_eq!(actual_rows.len(), 10, "no duplicate or missing owned predicate values");
                let actual = actual_rows.into_iter().collect::<std::collections::BTreeMap<_, _>>();
                assert_eq!(actual, expected, "all ten advertised document predicates retain exact canonical values");
            };
            assert_metadata(&before);
            assert_eq!(before["repair"], Value::Null);
            assert_eq!(before["documents"].as_array().unwrap().len(), 1);
            assert!(!before["documents"][0]["updateBase64"].as_str().unwrap().is_empty());
            let authority = || [
                "graph.json", "documents/seed-page/document.json",
                "documents/seed-page/.incarnation-id", "source-sync/ledger.json",
                "ydocs/documents/seed-page/update-v1.bin", "ydocs/workspace/update-v1.bin",
            ].into_iter().map(|p| (p, std::fs::read(graph_dir.join(p)).unwrap()))
                .collect::<Vec<_>>();
            let bytes_before = authority();
            let reseeds_before = crate::rdf_seed_service::reseed_count(&store_path);
            let documents_before =
                crate::rdf_seed_service::seed_document_materialization_count(&store_path);
            crate::rdf_seed_service::forget_process_seed_marker_for_test(&store_path);
            let after = pull(&app).await;
            let differences = before.as_object().unwrap().keys()
                .filter(|key| before[*key] != after[*key]).cloned().collect::<Vec<_>>();
            println!("SEED_REPLAY_COMPARISON={}", json!({
                "differentTopLevelFields": differences,
                "beforeManifest":before["manifest"]["sourceManifestHash"],
                "afterManifest":after["manifest"]["sourceManifestHash"],
                "beforeProjection":before["projectionSnapshot"]["digest"],
                "afterProjection":after["projectionSnapshot"]["digest"]
            }));
            assert_metadata(&after);
            assert_eq!(before, after, "full source bundle must survive a process seed-cache reset");
            assert_eq!(authority(), bytes_before, "cache reset must not change retained authority");
            assert_read_matches(&record, &read(&app, &record));
            assert_eq!(crate::rdf_seed_service::reseed_count(&store_path), reseeds_before);
            assert_eq!(crate::rdf_seed_service::seed_document_materialization_count(&store_path), documents_before);
            assert_eq!(pull(&app).await, after, "a subsequent warm pull is stable too");
            let explicit_walks_before = crate::rdf_seed_service::reseed_count(&store_path);
            let rebuilt = crate::source_sync::mcp_local_source_rebuild(app.clone(),
                &json!({"graphId":GRAPH,"graphIncarnation":initial["graphIncarnation"]}))
                .await.unwrap();
            assert_eq!(rebuilt["ok"], true, "two actual explicit replays remain idempotent");
            let after_rebuild = pull(&app).await;
            assert_metadata(&after_rebuild);
            assert_eq!(after_rebuild, after);
            let explicit_walks_after = crate::rdf_seed_service::reseed_count(&store_path);
            println!("SEED_REPLAY_WALK_COST={}", json!({
                "sourceWriteWalks":reseeds_before - source_write_walks_before,
                "coldAndWarmPullWalks":explicit_walks_before - reseeds_before,
                "twoExplicitReplayWalks":explicit_walks_after - explicit_walks_before,
            }));
            assert_eq!(explicit_walks_after - explicit_walks_before, 2,
                "each explicit old-checkpoint replay incurs exactly one reconciliation pass");
        });
    }));
    if let Some(value) = previous {
        std::env::set_var("GARDEN_PROFILE_DIR", value);
    } else {
        std::env::remove_var("GARDEN_PROFILE_DIR");
    }
    // Retain the one unique fixture for bounded authorized collection.
    if let Err(error) = outcome { std::panic::resume_unwind(error); }
}
