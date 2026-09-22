use super::*;
use oxigraph::model::{GraphName, Literal, NamedNode, Quad};

fn anchor() -> Value {
    serde_json::from_str::<Value>(include_str!("pdf_source_fixture.json")).unwrap()["anchor"]
        .clone()
}
fn body(raw: Value, text: &str) -> Value {
    json!({"type":"doc","content":[{"type":"paragraph","attrs":{
        "data-block-id":"pdf-block-1","data-pdf-anchor":raw},
        "content":[{"type":"text","text":text}]}]})
}
fn document(id: &str, raw: Value) -> DocumentRecord {
    serde_json::from_value(json!({"documentId":id,"graphId":"pdf-fixture",
        "title":"PDF fixture","body":"Aé\nB","origin":"local","providerId":"local",
        "localPath":"","rdfSubject":format!("urn:mnemosyne:local:document:{id}"),
        "createdAt":"2026-09-08T00:00:00Z","updatedAt":"2026-09-08T00:00:00Z",
        "capabilities":[],"tiptapJson":body(raw,"Aé\nB")}))
    .unwrap()
}
fn objects(store: &Store) -> BTreeSet<String> {
    store.iter().map(|q| q.unwrap().to_string()).collect()
}
fn has_literal(projection: &Projection, field: &str, value: &str) -> bool {
    projection.triples.iter().any(|(_, p, o)| {
        p == &format!("{NS}{field}") && matches!(o, Term::Lit(l) if l.value() == value)
    })
}

#[test]
fn pdf_source_shrubbery_portable_fixture_registry_and_conditional_annotation() {
    let raw = include_str!("pdf_source_shrubbery_fixture.json");
    assert_eq!(
        wire::sha256(raw),
        "b616f8fcffee759365e70b80ba57edfebe60d98449271926111159c36067e924"
    );
    let fixture: Value = serde_json::from_str(raw).unwrap();
    assert!(matches!(
        wire::inspect(Some(&fixture["wire"])),
        Inspection::Mapped(_)
    ));
    let mut doc = document("shared-fixture", fixture["wire"].clone());
    doc.tiptap_json = Some(body(
        fixture["wire"].clone(),
        fixture["text"].as_str().unwrap(),
    ));
    assert_eq!(
        wire::text_sha256(&doc.tiptap_json.as_ref().unwrap()["content"][0]),
        fixture["expectedTextSha256"].as_str().unwrap()
    );
    let p = project(&doc).unwrap();
    let oa_type = "http://www.w3.org/ns/oa#Annotation";
    assert_eq!(
        p.triples
            .iter()
            .filter(|(_, p, o)| p == crate::runtime_config::RDF_TYPE
                && matches!(o,Term::Uri(uri) if uri.as_str()==oa_type))
            .count(),
        1
    );
    assert_eq!(p.summary.rectangles_projected, 2);
    assert!(!has_literal(&p, "editedText", "true"));
    for raw in [json!("{bad"), json!("{\"version\":2}")] {
        doc.tiptap_json = Some(body(raw, "same source"));
        let p = project(&doc).unwrap();
        assert!(p
            .triples
            .iter()
            .any(|(_, p, _)| p == &format!("{NS}textBlock")));
        assert!(!p
            .triples
            .iter()
            .any(|(_, p, o)| p.starts_with("http://www.w3.org/ns/oa#")
                || matches!(o,Term::Uri(uri) if uri.as_str()==oa_type)));
        crate::emporium::shacl_validator::validate_desired(
            &p.triples,
            get_vocabulary(PACK).unwrap(),
        )
        .unwrap();
    }
    assert_eq!(
        wire::sha256(crate::emporium::vocabs::PDF_SOURCE_GOLDEN_JSON),
        crate::emporium::vocabs::PDF_SOURCE_GOLDEN_SHA
    );
    let contract = get_vocabulary(PACK).unwrap();
    for class in CLASSES {
        assert!(crate::emporium::class_dispatch::resolve(contract, class).is_ok());
    }
    assert!(crate::rdf_authority::validate_sparql_update_authority(
        "pdf-fixture",
        &format!(
            "INSERT DATA {{ GRAPH <{}> {{ <urn:forged> <urn:p> <urn:o> }} }}",
            sink("pdf-fixture")
        )
    )
    .is_err());
}

#[test]
fn pdf_source_typed_pack_shacl_and_exact_corruption_rebuild() {
    let contract = get_vocabulary(PACK).unwrap();
    assert!(contract.write_target.is_none());
    assert_eq!(contract.classes.len(), 4);
    let mut value = anchor();
    value["targets"][0]["rects"][0][0] = json!(10.0000001);
    let doc = document("doc-a", json!(value.to_string()));
    let projected = project(&doc).unwrap();
    assert_eq!(projected.summary.rectangles_projected, 2);
    assert!(has_literal(&projected, "mappingStatus", "mapped"));
    let precise = projected
        .triples
        .iter()
        .find(|(_, p, o)| {
            p == &format!("{NS}xMin") && matches!(o, Term::Lit(l) if l.value() == "10.0000001")
        })
        .unwrap()
        .clone();
    let store = Store::new().unwrap();
    reconcile(&store, &doc).unwrap();
    let baseline = objects(&store);
    let immediate_diff =
        reconcile_desired(&store, &doc.graph_id, &doc.document_id, &projected.triples).unwrap();
    assert!(
        immediate_diff.is_empty(),
        "exact immediate diff: {immediate_diff:#?}"
    );
    let quad = |value: &str| {
        Quad::new(
            NamedNode::new(&precise.0).unwrap(),
            NamedNode::new(&precise.1).unwrap(),
            Literal::new_typed_literal(
                value,
                NamedNode::new("http://www.w3.org/2001/XMLSchema#double").unwrap(),
            ),
            GraphName::NamedNode(NamedNode::new(sink(&doc.graph_id)).unwrap()),
        )
    };
    store.remove(&quad("10.0000001")).unwrap();
    store.insert(&quad("10.0000002")).unwrap();
    assert_ne!(objects(&store), baseline);
    // SAME source bytes, SAME selector identity, smaller than old 6-decimal diff.
    reconcile(&store, &doc).unwrap();
    assert_eq!(objects(&store), baseline);
    let bad: Vec<_> = projected
        .triples
        .iter()
        .filter(|(_, p, _)| p != &format!("{NS}documentId"))
        .cloned()
        .collect();
    assert!(crate::emporium::shacl_validator::validate_desired(&bad, contract).is_err());
}

#[test]
fn pdf_source_invalid_future_edit_and_document_ownership() {
    let store = Store::new().unwrap();
    let mut a = document("doc-a", json!(anchor().to_string()));
    let b = document("doc-b", json!(anchor().to_string()));
    reconcile(&store, &b).unwrap();
    let only_b = objects(&store);
    reconcile(&store, &a).unwrap();
    a.tiptap_json.as_mut().unwrap()["content"][0]["content"][0]["text"] = json!("edited");
    assert!(has_literal(&project(&a).unwrap(), "editedText", "true"));
    let mut future = anchor();
    future["version"] = json!(2);
    let raw = json!(future.to_string());
    a.tiptap_json = Some(body(raw.clone(), "edited"));
    let p = project(&a).unwrap();
    assert!(has_literal(&p, "mappingStatus", "unsupported"));
    assert_eq!(p.summary.rectangles_projected, 0);
    reconcile(&store, &a).unwrap();
    assert_eq!(
        a.tiptap_json.as_ref().unwrap()["content"][0]["attrs"][wire::ATTRIBUTE],
        raw
    );
    remove_document(&store, &a.graph_id, &a.document_id).unwrap();
    assert_eq!(objects(&store), only_b);
    a.tiptap_json = Some(body(json!("{malformed"), "edited"));
    assert!(has_literal(
        &project(&a).unwrap(),
        "mappingStatus",
        "invalid"
    ));
    a.tiptap_json = Some(body(Value::Null, "fresh recreation"));
    reconcile(&store, &a).unwrap();
    assert_eq!(objects(&store), only_b);
}

#[test]
fn pdf_source_budget_is_incomplete_not_invalid_source() {
    let mut a = anchor();
    a["targets"][0]["rects"] = json!(vec![[10, 40, 100, 60]; 2048]);
    let raw = a.to_string();
    assert!(matches!(
        wire::inspect(Some(&json!(raw))),
        Inspection::Mapped(_)
    ));
    let children: Vec<_> = (0..9)
        .map(|i| {
            let mut node = body(json!(raw), "Aé\nB")["content"][0].clone();
            node["attrs"]["data-block-id"] = json!(format!("block-{i}"));
            node
        })
        .collect();
    let mut doc = document("budget", Value::Null);
    doc.tiptap_json = Some(json!({"type":"doc","content":children}));
    let p = project(&doc).unwrap();
    assert_eq!(p.summary.rectangles_projected, MAX_DOCUMENT_RECTS);
    assert_eq!(p.summary.omitted_rectangles, 2048);
    assert!(!p.summary.complete);
    assert!(has_literal(&p, "mappingStatus", "budget-exceeded"));
    assert!(!has_literal(&p, "mappingStatus", "invalid"));
}

#[test]
fn pdf_source_real_queue_cold_tail_retry_queries_and_delete() {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let saved = std::env::var_os("GARDEN_PROFILE_DIR");
    let profile = std::env::temp_dir().join(format!("garden-pdf-source-{}", uuid::Uuid::new_v4()));
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let result = std::panic::catch_unwind(|| {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        let graph_id = "pdf-native-queue";
        let id = "pdf-queued-doc";
        crate::graph_service::create_graph_service(
            &app,
            crate::graph_service::CreateGraphInput {
                graph_id: Some(graph_id.into()),
                title: "PDF test".into(),
                description: None,
                operation_id: None,
            },
        )
        .unwrap();
        let write = |json_body: Value| {
            crate::app_runtime::async_runtime::block_on(crate::crdt_queue::enqueue_crdt_operation(
                app.clone(),
                crate::crdt_operation_types::EnqueueCrdtOperationInput {
                    kind: "document.write".into(),
                    graph_id: graph_id.into(),
                    document_id: Some(id.into()),
                    payload: json!({"documentId":id,"title":"PDF test","tiptapJson":json_body}),
                },
            ))
        };
        let raw = json!(anchor().to_string());
        fail_next_for_test(graph_id, id);
        // The executor retains a retryable tail fault and can repair it before
        // completing the live caller. Ok is not evidence the fault was absent.
        write(body(raw.clone(), "Aé\nB")).unwrap();
        let dir = crate::paths::existing_graph_dir(&app, graph_id).unwrap();
        let manifest = crate::paths::document_dir(&dir, id)
            .unwrap()
            .join("document.json");
        let cold =
            crate::document_record_store::read_document_record_cold(&dir, &manifest).unwrap();
        assert!(cold.ydoc_update_base64.is_empty());
        assert_eq!(
            cold.tiptap_json.as_ref().unwrap()["content"][0]["attrs"][wire::ATTRIBUTE],
            raw
        );
        let revision = cold.revision;
        let mut marker = crate::document_history_file_store::read_document_tail_commit(&dir, id)
            .unwrap()
            .unwrap();
        marker.schema_version = 1;
        crate::document_history_file_store::write_document_tail_commit(&dir, &marker).unwrap();
        fail_next_for_test(graph_id, id);
        assert!(
            crate::document_persistence_service::ensure_document_persistence_tail(&dir, &cold)
                .is_err()
        );
        assert_eq!(
            crate::document_history_file_store::read_document_tail_commit(&dir, id)
                .unwrap()
                .unwrap()
                .schema_version,
            1
        );
        assert!(
            crate::document_persistence_service::ensure_document_persistence_tail(&dir, &cold)
                .unwrap()
        );
        assert!(
            !crate::document_persistence_service::ensure_document_persistence_tail(&dir, &cold)
                .unwrap()
        );
        let hydrated = crate::document_record_store::read_document_record(&dir, &manifest).unwrap();
        assert!(!hydrated.ydoc_update_base64.is_empty());
        assert_eq!(hydrated.revision, revision);
        {
            use base64::Engine;
            use yrs::{updates::decoder::Decode, Transact};
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&hydrated.ydoc_update_base64)
                .unwrap();
            let decoded = yrs::Doc::new();
            decoded
                .transact_mut()
                .apply_update(yrs::Update::decode_v1(&bytes).unwrap())
                .unwrap();
            let json = crate::crdt_engine::projection::ydoc_to_tiptap_json(&decoded);
            assert_eq!(json["content"][0]["attrs"][wire::ATTRIBUTE], raw);
        }
        let store = crate::rdf_store_service::open_graph_store(&dir).unwrap();
        let desired = project(&cold).unwrap();
        assert!(reconcile_desired(&store, graph_id, id, &desired.triples)
            .unwrap()
            .is_empty());
        let anchor_iri = desired
            .triples
            .iter()
            .find(|(_, p, o)| {
                p == crate::runtime_config::RDF_TYPE
                    && matches!(o,Term::Uri(uri) if uri.as_str()==format!("{NS}TextSourceAnchor"))
            })
            .unwrap()
            .0
            .clone();
        let query = crate::emporium::object_query::run_object_query(
            &app,
            graph_id,
            PACK,
            "TextSourceAnchor",
            &json!({}),
            &crate::emporium::object_query::ObjectQueryOptions::default(),
        )
        .unwrap();
        assert_eq!(query.objects.len(), 1);
        let named = crate::emporium::query_engine::run_named_query(
            &app,
            graph_id,
            PACK,
            "TextSourceAnchor",
            "byId",
            None,
            &json!({"subject":anchor_iri}),
        )
        .unwrap();
        assert!(!named.rows.is_empty());
        let joins = crate::rdf_query_service::execute_sparql_query(&store,&format!(
            "SELECT ?block ?original ?selector ?page WHERE {{ GRAPH <{}> {{ <{}> <http://www.w3.org/ns/oa#hasBody> ?block ; <http://www.w3.org/ns/oa#hasTarget> ?region . ?region <http://www.w3.org/ns/oa#hasSource> ?original ; <http://www.w3.org/ns/oa#hasSelector> ?selector . ?selector <{}pageIndex> ?page . }} }}",sink(graph_id),anchor_iri,NS)).unwrap();
        assert_eq!(joins.rows.len(), 2);
        assert!(crate::app_runtime::async_runtime::block_on(
            crate::emporium::write::emporium_write(
                &app,
                graph_id,
                PACK,
                &[json!({"kind":"TextSourceAnchor","localId":"forged"})],
                false,
                false,
                None
            )
        )
        .is_err());
        assert!(crate::app_runtime::async_runtime::block_on(
            crate::emporium::write::emporium_retract(
                &app,
                graph_id,
                &anchor_iri,
                "not authored",
                "retract",
                None
            )
        )
        .is_err());
        assert!(crate::app_runtime::async_runtime::block_on(
            crate::emporium::objects::update_object(
                &app,
                graph_id,
                PACK,
                "TextSourceAnchor",
                &anchor_iri,
                json!({"kind":"TextSourceAnchor"})
            )
        )
        .is_err());
        assert!(crate::app_runtime::async_runtime::block_on(
            crate::emporium::objects::delete_object(
                &app,
                graph_id,
                PACK,
                "TextSourceAnchor",
                &anchor_iri
            )
        )
        .is_err());
        // A genuinely cold projection sink, with no marker, repairs unchanged
        let request: crate::emporium::schemas::IngestRequest = serde_json::from_value(json!({
            "vocab":PACK,"dry_run":false,"payload":{"kind":"generic","records":[{
                "kind":"PdfOriginal","localId":"forged","document":cold.rdf_subject,
                "documentId":id,"sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "byteLength":1234,"attestation":"declared-source-unverified"
            }]}})).unwrap();
        let planned = crate::emporium::spine::gather_and_plan(&app, graph_id, &request).unwrap();
        let report =
            crate::app_runtime::async_runtime::block_on(crate::emporium::spine::apply_and_assert(
                &app,
                graph_id,
                &planned.plan,
                planned.contract,
                &request,
            ));
        assert!(
            !report.ok,
            "generic ingest must not author a derived PDF object"
        );
        assert!(reconcile_desired(&store, graph_id, id, &desired.triples)
            .unwrap()
            .is_empty());
        // A genuinely cold projection sink, with no marker, repairs unchanged
        // persisted document source without incrementing its revision.
        let seed = crate::rdf_authority::seed_marker_graph_iri(graph_id);
        let to_remove: Vec<_> = store.iter().map(Result::unwrap).filter(|q| {
            matches!(&q.graph_name, GraphName::NamedNode(n) if n.as_str() == sink(graph_id) || n.as_str() == seed)
        }).collect();
        for quad in to_remove {
            store.remove(&quad).unwrap();
        }
        crate::rdf_seed_service::forget_process_seed_marker_for_test(&dir.join("store.oxigraph"));
        crate::rdf_seed_service::ensure_graph_store_seeded(&dir).unwrap();
        assert!(reconcile_desired(&store, graph_id, id, &desired.triples)
            .unwrap()
            .is_empty());
        assert_eq!(
            crate::document_record_store::read_document_record_cold(&dir, &manifest)
                .unwrap()
                .revision,
            revision
        );
        crate::document_delete_service::delete_document_for_operation(
            app.clone(),
            graph_id.into(),
            id.into(),
            None,
        )
        .unwrap();
        assert!(reconcile_desired(&store, graph_id, id, &[])
            .unwrap()
            .is_empty());
        write(body(Value::Null, "recreated without source mapping")).unwrap();
        assert!(reconcile_desired(&store, graph_id, id, &[])
            .unwrap()
            .is_empty());
        // Exact bytes + real PDF.js/TipTap producer output supplied by Sophia.
        // Bytes are checked here as a synthetic oracle, not fetched/attested by
        // the projector or promoted to an artifact-revision authority.
        let input_raw = include_str!("pdf_source_parser_fixture.json");
        assert_eq!(
            wire::sha256(input_raw),
            "e5554e41d55f38a903292be40e717c635d3fe2c425e700fcb04748658668e746"
        );
        let input: Value = serde_json::from_str(input_raw).unwrap();
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(input["originalBase64"].as_str().unwrap())
            .unwrap();
        write(input["tiptapJson"].clone()).unwrap();
        let actual =
            crate::document_record_store::read_document_record_cold(&dir, &manifest).unwrap();
        let mut stack = vec![actual.tiptap_json.as_ref().unwrap()];
        let mut expected = Vec::new();
        let mut anchors = 0;
        while let Some(node) = stack.pop() {
            if let Some(children) = node.get("content").and_then(Value::as_array) {
                stack.extend(children);
            }
            let Some(raw) = node
                .get("attrs")
                .and_then(|v| v.get(wire::ATTRIBUTE))
                .filter(|v| !v.is_null())
            else {
                continue;
            };
            let Inspection::Mapped(anchor) = wire::inspect(Some(raw)) else {
                panic!("actual parser anchor must map")
            };
            anchors += 1;
            assert_eq!(anchor.source.sha256, wire::sha256(&bytes));
            assert_eq!(anchor.source.byte_length, bytes.len() as f64);
            assert_eq!(anchor.text_sha256, wire::text_sha256(node));
            for target in anchor.targets {
                for rect in target.rects {
                    expected.push(vec![
                        target.page_index,
                        rect[0],
                        rect[1],
                        rect[2],
                        rect[3],
                        target.view_box[0],
                        target.view_box[1],
                        target.view_box[2],
                        target.view_box[3],
                        target.user_unit,
                        target.rotation,
                    ]);
                }
            }
        }
        assert!(anchors >= 2);
        let fields = [
            "pageIndex",
            "xMin",
            "yMin",
            "xMax",
            "yMax",
            "cropXMin",
            "cropYMin",
            "cropXMax",
            "cropYMax",
            "userUnit",
            "rotation",
        ];
        let variables: Vec<_> = (0..fields.len()).map(|i| format!("?v{i}")).collect();
        let pattern = fields
            .iter()
            .zip(&variables)
            .map(|(p, v)| format!("<{NS}{p}> {v}"))
            .collect::<Vec<_>>()
            .join(" ; ");
        let rows = crate::rdf_query_service::execute_sparql_query(
            &store,
            &format!(
                "SELECT {} WHERE {{ GRAPH <{}> {{ ?s a <{NS}PdfPageSelector> ; {pattern} . }} }}",
                variables.join(" "),
                sink(graph_id)
            ),
        )
        .unwrap();
        let mut observed: Vec<Vec<f64>> = rows
            .rows
            .iter()
            .map(|row| {
                (0..fields.len())
                    .map(
                        |i| match crate::emporium::survey::parse_term(&row[&format!("v{i}")]) {
                            Term::Lit(l) => l.value().parse::<f64>().unwrap(),
                            _ => panic!("numeric selector"),
                        },
                    )
                    .collect()
            })
            .collect();
        expected.sort_by(|a, b| a.partial_cmp(b).unwrap());
        observed.sort_by(|a, b| a.partial_cmp(b).unwrap());
        assert_eq!(observed, expected);
        println!("PDF producer/native join: {anchors} source anchors, {} exact selector rows, {} original bytes",observed.len(),bytes.len());
        drop(store);
        crate::rdf_store_service::evict_graph_store(&dir).unwrap();
    });
    match saved {
        Some(value) => std::env::set_var("GARDEN_PROFILE_DIR", value),
        None => std::env::remove_var("GARDEN_PROFILE_DIR"),
    }
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}
