//! Independently authored consumer assertions; shared helpers are transport only.
use super::*;
use std::{collections::BTreeMap, io::Read, path::Path};

fn specimen(name: &str) -> (Vec<u8>, Value) {
    let inputs=std::env::var("GARDEN_PRESERVATION_EXAMINER_INPUTS").expect("explicit retained examiner input directory required; no default profile");
    let bytes = std::fs::read(Path::new(&inputs).join(format!("{name}.tar.gz"))).expect("named retained examiner archive missing");
    let plan = json!({"operationId":format!("independent-v2-{name}"),
        "sourceUserId":"fixture-cloud1-owner-20260908","sourceGraphId":GRAPH,
        "targetGeneration":1,"archiveSha256":format!("{:x}",Sha256::digest(&bytes)),
        "expectedDocumentCount":2,"expectedRdfTripleCount":11,"formatVersion":2});
    (bytes,plan)
}

fn wire(bytes: &[u8], plan: &Value) -> Vec<u8> {
    let mut fields = plan.clone();
    fields["planDigest"] = json!(format!("{:x}",Sha256::digest(serde_json::to_vec(plan).unwrap())));
    let mut output = Vec::new();
    for (key,value) in fields.as_object().unwrap() {
        let value=value.as_str().map(str::to_owned).unwrap_or_else(||value.to_string());
        output.extend_from_slice(format!("--restore-boundary\r\nContent-Disposition: form-data; name=\"{key}\"\r\n\r\n{value}\r\n").as_bytes());
    }
    output.extend_from_slice(b"--restore-boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"independent.tar.gz\"\r\nContent-Type: application/gzip\r\n\r\n");
    output.extend_from_slice(bytes);
    output.extend_from_slice(b"\r\n--restore-boundary--\r\n");
    output
}

fn members(bytes: &[u8]) -> BTreeMap<String,Vec<u8>> {
    let mut archive=tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    archive.entries().unwrap().map(|entry| {
        let mut entry=entry.unwrap();let name=entry.path().unwrap().to_str().unwrap().to_owned();
        let mut body=Vec::new();entry.read_to_end(&mut body).unwrap();(name,body)
    }).collect()
}

fn content_tree(bytes: &[u8]) -> Value {
    use yrs::{Any,Doc,ReadTxn,Text,Transact,Update,Xml};
    use yrs::types::xml::{XmlFragment,XmlOut};
    use yrs::updates::decoder::Decode;
    fn any(value:&Any)->Value {
        match value {
            Any::Null=>json!(["null"]),Any::Undefined=>json!(["undefined"]),
            Any::Bool(v)=>json!(["bool",v]),Any::Number(v)=>json!(["number",v]),
            Any::BigInt(v)=>json!(["bigint",v.to_string()]),Any::String(v)=>json!(["string",v.as_ref()]),
            Any::Buffer(v)=>json!(["bytes",v.as_ref()]),
            Any::Array(v)=>json!(["array",v.iter().map(any).collect::<Vec<_>>()]),
            Any::Map(v)=>json!(["map",v.iter().map(|(k,v)|(k.to_string(),any(v))).collect::<BTreeMap<_,_>>()]),
        }
    }
    fn node<T:ReadTxn>(txn:&T,value:XmlOut)->Value {
        match value {
            XmlOut::Element(e)=>json!({"element":e.tag().to_string(),
                "attributes":e.attributes(txn).map(|(k,v)| {
                    let yrs::Out::Any(v)=v else {panic!("unsupported attribute carrier")};
                    (k.to_string(),any(&v))
                }).collect::<BTreeMap<_,_>>(),
                "children":e.children(txn).map(|v|node(txn,v)).collect::<Vec<_>>()}),
            XmlOut::Text(t)=>json!({"text":t.diff(txn,yrs::types::text::YChange::identity).into_iter().map(|diff| {
                let yrs::Out::Any(Any::String(text))=diff.insert else {panic!("unsupported text embed")};
                let marks=diff.attributes.as_deref().map(|attrs|attrs.iter().map(|(k,v)|(k.to_string(),any(v))).collect::<BTreeMap<_,_>>()).unwrap_or_default();
                json!({"text":text.as_ref(),"marks":marks})
            }).collect::<Vec<_>>()}),
            XmlOut::Fragment(f)=>json!({"fragment":f.children(txn).map(|v|node(txn,v)).collect::<Vec<_>>()}),
        }
    }
    let doc=Doc::new();
    doc.transact_mut().apply_update(Update::decode_v1(bytes).unwrap()).unwrap();
    let txn=doc.transact();
    match txn.get_xml_fragment("content") {
        Some(root)=>json!(root.children(&txn).map(|v|node(&txn,v)).collect::<Vec<_>>()),
        None=>json!([]),
    }
}

// Consumer-side source reconstruction: do not call the converter's wire rule or
// graph mapping helper. The independent runtime reader separately checks anatomy.
fn source_only_wire_readback(graph_dir: &Path, user: &str, graph: &str, digest: &str,
    source: &BTreeMap<String,Vec<u8>>, imported: &Value) -> Value {
    use std::collections::BTreeSet;
    use yrs::{Doc,Map,ReadTxn,Transact,Update};
    use yrs::updates::decoder::Decode;
    use oxigraph::{io::{RdfFormat,RdfParser},model::{NamedNode,NamedOrBlankNode,GraphName}};
    let doc=Doc::new();
    doc.transact_mut().apply_update(Update::decode_v1(&source["crdt/workspace.yjs"]).unwrap()).unwrap();
    let txn=doc.transact();
    let saved:BTreeSet<String>=txn.get_map("wires").map(|m|m.iter(&txn).map(|(id,value)| {
        assert!(matches!(value,yrs::Out::YMap(_)),"source wire carrier must be typed map");id.to_string()
    }).collect()).unwrap_or_default();
    let record=crate::document_service::read_workspace_record(graph_dir,graph).unwrap();
    let native:BTreeSet<String>=crate::workspace_entity_projection::workspace_wires(record.snapshot.as_ref())
        .iter().map(|r|r["id"].as_str().unwrap().to_string()).collect();
    assert_eq!(native,saved,"exact saved/native wire identity set; no evidence materialization");
    let main=format!("urn:mnemosyne:user:{user}:graph:{graph}");
    let prefix=format!("{main}:wire:");
    let evidence_prefix=format!("urn:mnemosyne:local:graph:{graph}:user:legacy-evidence:");
    let context=serde_json::to_vec(&json!([user,graph,digest,main])).unwrap();
    let evidence_graph=format!("{evidence_prefix}{:x}",Sha256::digest(&context));
    let mut expected=BTreeSet::new();let mut source_quads=BTreeSet::new();let mut absent=BTreeSet::new();
    for q in RdfParser::from_format(RdfFormat::NQuads).for_slice(&source["rdf/dataset.nq"]) {
        let mut q=q.unwrap();
        let NamedOrBlankNode::NamedNode(subject)=&q.subject else {continue;};
        let Some(id)=subject.as_str().strip_prefix(&prefix) else {continue;};
        if saved.contains(id) {continue;}
        assert!(matches!(&q.graph_name,GraphName::NamedNode(n) if n.as_str()==main));
        absent.insert(format!("urn:mnemosyne:local:graph:{graph}:wire:{id}"));
        source_quads.insert(format!("{q} .\n"));
        q.graph_name=NamedNode::new(&evidence_graph).unwrap().into();expected.insert(q.to_string());
    }
    let entries=imported["sourceDerivedAssertionDisposition"]["entries"].as_array().unwrap();
    let mut declared=BTreeSet::new();
    for row in entries.iter().filter(|r|r["evidenceGraph"].is_string()) {
        assert_eq!(row["reason"],"source-only-wire-anatomy-v1");
        assert_eq!(row["evidenceGraph"],evidence_graph);assert_eq!(row["sourceGraphIri"],main);
        assert_eq!(row["sourceArchiveSha256"],digest);assert_eq!(row["currentEntityExistenceAsserted"],false);
        assert_eq!(row["referenceOnly"],true);assert!(row["native"].is_null());
        let quad=row["sourceQuad"].as_str().unwrap();
        assert_eq!(row["sourceQuadCanonicalSha256"],format!("{:x}",Sha256::digest(quad.as_bytes())));
        assert!(declared.insert(quad.to_string()),"no duplicate source disposition");
    }
    assert_eq!(declared,source_quads,"complete source-only quad coverage, not sampled claims");
    assert_eq!(imported["rdfPreservationAccounting"]["nativeEvidenceQuadCount"],expected.len());
    let store=crate::rdf_store_service::open_graph_store(graph_dir).unwrap();
    let mut actual=BTreeSet::new();
    for q in store.iter() {
        let q=q.unwrap();
        assert!(!matches!(&q.subject,NamedOrBlankNode::NamedNode(n) if absent.contains(n.as_str())),"no phantom native wire projection");
        if matches!(&q.subject,NamedOrBlankNode::NamedNode(n) if n.as_str().strip_prefix(&prefix).is_some_and(|id|!saved.contains(id))) {
            assert!(matches!(&q.graph_name,GraphName::NamedNode(n) if n.as_str()==evidence_graph),"source-only subjects stay outside current authority");
        }
        if matches!(&q.graph_name,GraphName::NamedNode(n) if n.as_str().starts_with(&evidence_prefix)) {
            actual.insert(q.to_string());
        }
    }
    assert_eq!(actual,expected,"actual retained named graph fullset, including no-evidence control");
    let query=format!("SELECT ?s ?p ?o WHERE {{ GRAPH <{evidence_graph}> {{ ?s ?p ?o }} }}");
    let oxigraph::sparql::QueryResults::Solutions(rows)=store.query(query.as_str()).unwrap() else {panic!("expected query rows");};
    assert_eq!(rows.map(Result::unwrap).count(),expected.len());
    json!({"savedWireCount":saved.len(),"nativeWireCount":native.len(),"sourceOnlyWireCount":absent.len(),
        "evidenceQuadCount":expected.len(),"completeSourceQuadCoverage":true,"actualStoreFullSet":true,
        "explicitNamedGraphQuery":true,"noCurrentWireMaterialization":true})
}

// Actual retained inputs, but exclusively an isolated mock authority and private
// profile. No production lease, account call, or runtime lifecycle is exercised.
fn actual_rich_signed_profile(graph: &str, input_env: &str, digest: &str, documents: u64, quads: u64, histories: usize) {
    actual_rich_signed_profile_owned("e9e949fe-0091-7015-0ab8-10bf259084ab", graph, input_env, digest, documents, quads, histories);
}

fn actual_rich_signed_profile_owned(user: &str, graph: &str, input_env: &str, digest: &str, documents: u64, quads: u64, histories: usize) {
    let clock=std::time::Instant::now();
    let phase=|name:&str| eprintln!("POPULATION_PHASE {} {}",name,clock.elapsed().as_millis());
    phase("start");
    let _serial=crate::tauri_runtime::profile_env_serial().lock().unwrap_or_else(|p|p.into_inner());
    let bytes=std::fs::read(std::env::var(input_env).expect("explicit retained archive required")).unwrap();
    assert_eq!(format!("{:x}",Sha256::digest(&bytes)),digest,"exact retained input");
    let source=members(&bytes);
    let manifest:Value=serde_json::from_slice(&source["manifest.json"]).unwrap();
    assert_eq!(manifest["source"]["userId"],user);
    assert_eq!(manifest["source"]["graphId"],graph);
    assert_eq!(manifest["counts"]["documents"],documents);
    assert_eq!(manifest["counts"]["rdfQuads"],quads);
    let owner=format!("user:{user}");
    let base=std::env::temp_dir().join(format!("garden-actual-rich-{}",uuid::Uuid::new_v4()));
    std::fs::create_dir(&base).unwrap();
    let profile=base.join("profile");
    let previous=std::env::var_os("GARDEN_PROFILE_DIR");
    std::env::set_var("GARDEN_PROFILE_DIR",&profile);
    let result=std::panic::catch_unwind(|| {
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap().block_on(async {
            let state=state_for(&profile,&owner,graph);
            phase("state-created");
            let before=crate::graph_service::create_graph_service(&state.app,crate::graph_service::CreateGraphInput {
                graph_id:Some(graph.into()),title:"Isolated retained compatibility fixture".into(),description:None,operation_id:None,
            }).unwrap();
            phase("graph-created");
            let mut plan=json!({"operationId":format!("actual-rich-{graph}"),"sourceUserId":user,"sourceGraphId":graph,
                "targetGeneration":1,"archiveSha256":digest,"expectedDocumentCount":documents,
                "expectedRdfTripleCount":quads,"formatVersion":2});
            // The import declares its own admission standard. Absent is strict; the env var
            // carries the conceded kinds as the JSON array a real plan would carry, so one
            // trial measures both standards without a second code path.
            if let Ok(parity)=std::env::var("GARDEN_PRESERVATION_CONTENT_PARITY") {
                plan["contentParity"]=serde_json::from_str(&parity).expect("contentParity must be a JSON array");
            }
            let plan=plan;
            let path=format!("/graphs/{graph}/restore-archive");
            let mime="multipart/form-data; boundary=restore-boundary";
            let (status,_)=request(state.clone(),&path,&lease_for(&owner,graph,"owner",Some(("owner",json!("user:foreign")))),wire(&bytes,&plan),mime).await;
            assert_eq!(status,StatusCode::UNAUTHORIZED);
            phase("wrong-owner-refused");
            let graph_dir=crate::graph_paths::existing_graph_dir(&state.app,graph).unwrap();
            assert!(!graph_dir.join(".migration/preservation-v2/claim.json").exists());
            let (status,body)=request(state.clone(),&path,&lease_for(&owner,graph,"owner",None),wire(&bytes,&plan),mime).await;
            assert_eq!(status,StatusCode::ACCEPTED,"actual signed import admission");
            phase("signed-request-accepted");
            let job=await_job_for(&state,&body,600).await;
            phase("job-terminal");
            assert!(matches!(job.status,LocalJobStatus::Succeeded),"actual import failed: {:?}",job.error);
            assert_eq!(job.owner_principal.as_deref(),Some(owner.as_str()));
            let imported=job.detail.get("result_inline").cloned()
                .or_else(||state.jobs.read_spilled_result(&job).unwrap())
                .expect("complete successful result, inline or existing spill contract");
            assert_eq!(imported["rdfPreservationAccounting"]["complete"],true);
            let accounting=&imported["rdfPreservationAccounting"];
            let exact=accounting["exactMappedSourceCount"].as_u64().unwrap();
            let normalized=accounting["normalizedSourceCount"].as_u64().unwrap();
            let retained=accounting["legacyEvidenceOnlySourceCount"].as_u64().unwrap();
            assert_eq!(exact+normalized+retained,quads);
            assert_eq!(accounting["sourceQuadCount"],quads);
            assert_eq!(imported["rdfMappedTestimonyFullSet"],exact==quads);
            assert_eq!(accounting["rdfMappedTestimonyFullSet"],exact==quads);
            let (_,after)=crate::graph_service::read_graph_record(&state.app,graph).unwrap();
            assert_eq!(before.incarnation_id,after.incarnation_id);
            let custody=graph_dir.join(".migration/preservation-v2");
            assert_eq!(std::fs::read(custody.join("archive.tar.gz")).unwrap(),bytes);
            for (path,expected) in &source {
                assert_eq!(std::fs::read(custody.join("source").join(path)).unwrap(),*expected,"source member custody");
            }
            phase("source-custody-compared");
            let mut seen=0;
            for (path,expected) in &source {
                if let Some(id)=path.strip_prefix("crdt/documents/").and_then(|p|p.strip_suffix(".yjs")) {
                    let actual=std::fs::read(crate::paths::document_ydoc_state_path(&graph_dir,id)).unwrap();
                    assert_eq!(content_tree(&actual),content_tree(expected),"exact current document tree");
                    assert!(crate::document_service::read_document(state.app.clone(),graph.into(),id.into()).is_ok());
                    seen+=1;
                }
            }
            let unavailable = imported["unavailableBodies"].as_array().expect("explicit unavailable inventory");
            phase("current-bodies-read");
            assert_eq!(seen + unavailable.len() as u64,documents);
            assert_eq!(imported["metadataDocumentCount"], documents);
            assert_eq!(imported["presentDocumentBodyCount"], seen);
            assert_eq!(imported["documentCount"], seen);
            let custody_index:Value=serde_json::from_slice(&source["source-custody/index.json"]).unwrap();
            assert_eq!(unavailable.len(), custody_index.get("unavailableBodies").and_then(Value::as_array).map_or(0,Vec::len));
            let listing=serde_json::to_value(crate::document_hosted_projection::hosted_document_summaries(&state.app,graph).unwrap()).unwrap();
            assert_eq!(listing.as_array().unwrap().len() as u64,documents);
            for row in unavailable {
                let id=row["documentId"].as_str().unwrap();
                let entry=listing.as_array().unwrap().iter().find(|entry|entry["id"]==id).unwrap();
                assert!(entry["revision"].is_null());
                assert_eq!(entry["readOnly"],true);
                assert_eq!(entry["bodyAvailability"]["sourceUserId"],user);
                assert_eq!(entry["bodyAvailability"]["sourceGraphId"],graph);
                assert_eq!(entry["bodyAvailability"]["inventorySha256"],row["inventorySha256"]);
                assert!(crate::document_service::read_document(state.app.clone(),graph.into(),id.into())
                    .unwrap_err().starts_with("source_body_unavailable:"));
                assert!(!crate::paths::document_ydoc_state_path(&graph_dir,id).exists());
                assert!(!crate::document_tombstone_store::document_is_tombstoned(&graph_dir,id).unwrap());
            }
            let payloads=imported["documentHistoryPayloads"].as_array().expect("native history witnesses");
            phase("metadata-unavailable-reads-compared");
            let disposition=&imported["documentHistoryDisposition"];
            assert_eq!(disposition["schema"],"cloud1-document-history-disposition.v1");
            let native=disposition["nativeInterpretedCount"].as_u64().unwrap() as usize;
            let legacy=disposition["legacyReadOnlyCount"].as_u64().unwrap() as usize;
            let missing=disposition["unavailablePayloadCount"].as_u64().unwrap() as usize;
            assert_eq!(payloads.len(),native);
            assert_eq!(native+legacy,histories,"available source history must be native or disclosed legacy");
            let entries=disposition["entries"].as_array().unwrap();
            assert_eq!(entries.len(),native+legacy+missing);
            assert_eq!(disposition["retainedMetadataCount"],entries.len());
            for (status,count) in [("native-interpreted",native),("legacy-read-only",legacy),("source-payload-unavailable",missing)] {
                assert_eq!(entries.iter().filter(|entry|entry["status"]==status).count(),count);
            }
            for entry in entries {
                let present=payloads.iter().filter(|row|row["documentId"]==entry["documentId"] && row["snapshotId"]==entry["snapshotId"]).count();
                assert_eq!(present,usize::from(entry["status"]=="native-interpreted"));
                if entry["status"]!="native-interpreted" {
                    assert_eq!(entry["nativeSnapshotCreated"],false);
                    assert_eq!(entry["nativeRestorable"],false);
                }
            }
            let proof=&imported["documentHistoryDispositionFile"];
            let raw=std::fs::read(graph_dir.join(proof["path"].as_str().unwrap())).unwrap();
            assert_eq!(proof["byteLength"],raw.len());
            assert_eq!(proof["sha256"],format!("{:x}",Sha256::digest(&raw)));
            assert_eq!(serde_json::from_slice::<Value>(&raw).unwrap(),*disposition);
            for row in payloads {
                let body=std::fs::read(graph_dir.join(row["path"].as_str().unwrap())).unwrap();
                assert_eq!(row["byteLength"],body.len());
                assert_eq!(row["sha256"],format!("{:x}",Sha256::digest(&body)));
            }
            let originals:Value=serde_json::from_slice(&source["originals/index.json"]).unwrap();
            for row in originals.as_array().unwrap() {
                let identity=row["id"].as_str().unwrap();
                let (_,actual)=match row["ownerKind"].as_str().unwrap() {
                    "document"=>crate::original_file_service::read_document_original_file(&state.app,graph,identity),
                    "artifact"=>crate::original_file_service::read_artifact_original_file(&state.app,graph,identity),
                    "image"=>crate::original_file_service::read_image_file(&state.app,graph,identity),
                    _=>panic!("unsupported source original owner kind"),
                }.unwrap();
                assert_eq!(actual,source[row["member"].as_str().unwrap()]);
            }
            phase("history-originals-compared");
            let evidence=source_only_wire_readback(&graph_dir,user,graph,digest,&source,&imported);
            std::fs::write(base.join("source-only-first-read.json"),serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();
            std::fs::write(base.join("result.json"),serde_json::to_vec_pretty(&imported).unwrap()).unwrap();
            std::fs::write(base.join("qualification.json"),serde_json::to_vec_pretty(&json!({"graphId":graph,"ownerPrincipal":owner,
                "archiveSha256":digest,"documents":documents,"histories":histories,"profile":"profile",
                "signedImport":true,"liveAuthority":false})).unwrap()).unwrap();
            phase("result-written");
        });
        phase("first-runtime-closed");
        // New runtime/reader context after the importer and its tasks are gone.
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap().block_on(async {
            let cold=state_for(&profile,&owner,graph);
            phase("reopen-state-created");
            let graph_dir=crate::graph_paths::existing_graph_dir(&cold.app,graph).unwrap();
            let imported:Value=serde_json::from_slice(&std::fs::read(base.join("result.json")).unwrap()).unwrap();
            let evidence=source_only_wire_readback(&graph_dir,user,graph,digest,&source,&imported);
            let first:Value=serde_json::from_slice(&std::fs::read(base.join("source-only-first-read.json")).unwrap()).unwrap();
            assert_eq!(evidence,first);
            std::fs::write(base.join("source-only-reopen-read.json"),serde_json::to_vec_pretty(&evidence).unwrap()).unwrap();
            for row in crate::document_body_availability::unavailable_documents(&graph_dir).unwrap() {
                let id=row["documentId"].as_str().unwrap();
                assert!(crate::document_service::read_document(cold.app.clone(),graph.into(),id.into())
                    .unwrap_err().starts_with("source_body_unavailable:"));
            }
            for path in source.keys() {
                if let Some(id)=path.strip_prefix("crdt/documents/").and_then(|p|p.strip_suffix(".yjs")) {
                    assert!(crate::document_service::read_document(cold.app.clone(),graph.into(),id.into()).is_ok());
                }
            }
        });
        phase("reopen-runtime-closed");
    });
    match previous {Some(value)=>std::env::set_var("GARDEN_PROFILE_DIR",value),None=>std::env::remove_var("GARDEN_PROFILE_DIR")}
    eprintln!("Actual rich private profile retained: {}",base.display());
    if let Err(error)=result {std::panic::resume_unwind(error);}
}

#[test]
fn population_manifest_signed_import_profile() {
    let file = std::env::var("GARDEN_PRESERVATION_PROFILE_MANIFEST").expect("explicit profile manifest required");
    let row: Value = serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
    actual_rich_signed_profile_owned(row["sourceOwner"].as_str().unwrap(), row["graphId"].as_str().unwrap(),
        "GARDEN_PRESERVATION_RETAINED_ARCHIVE", row["archiveSha256"].as_str().unwrap(),
        row["documents"].as_u64().unwrap(), row["rdfQuads"].as_u64().unwrap(),
        row["histories"].as_u64().unwrap() as usize);
}

#[test]
fn actual_wf_emit_lab_signed_import_profile() {
    actual_rich_signed_profile("wf-emit-lab","GARDEN_PRESERVATION_HISTORY_ARCHIVE_08",
        "3d092a7b9609fde7f5165cf497e236d2ab6d440c8b2404c80abbc32281fc212c",32,3289,39);
}

#[test]
fn actual_workflow_commons_copy_unavailable_signed_import_profile() {
    actual_rich_signed_profile("workflow-commons-copy","GARDEN_PRESERVATION_UNAVAILABLE_ARCHIVE",
        "d5f7cfae99926e3acea0e47a252bb6db473d11e95a86000530cdb0d67b23ff6c",31,10314,0);
}

#[test]
fn actual_choreograph_2_signed_import_profile() {
    actual_rich_signed_profile("choreograph-2","GARDEN_PRESERVATION_CANDIDATE23_ARCHIVE",
        "ecca0ebf97e31679420fea82eb5317933046bd23f1e7d968a6756d3f8851302f",62,124770,6);
}

#[test]
fn independent_v2_tree_comparator_retains_attribute_values() {
    use yrs::{Doc,ReadTxn,StateVector,Transact,Update,Xml};
    use yrs::types::xml::{XmlFragment,XmlOut};
    use yrs::updates::decoder::Decode;
    let (archive,_)=specimen("healthy");let source=members(&archive);
    let bytes=&source["crdt/documents/doc-fixture-alpha.yjs"];
    let doc=Doc::new();doc.transact_mut().apply_update(Update::decode_v1(bytes).unwrap()).unwrap();
    {
        let mut txn=doc.transact_mut();
        let root=txn.get_xml_fragment("content").unwrap();
        let XmlOut::Element(heading)=root.children(&txn).next().unwrap() else {panic!("source heading")};
        heading.insert_attribute(&mut txn,"level","2");
    }
    let changed=doc.transact().encode_state_as_update_v1(&StateVector::default());
    assert_ne!(content_tree(bytes),content_tree(&changed),"attribute values must not be normalized away");
}

#[test]
fn independent_v2_real_caller_preserves_and_refuses() {
    let _serial=crate::tauri_runtime::profile_env_serial().lock().unwrap_or_else(|p|p.into_inner());
    let base=std::env::temp_dir().join(format!("garden-independent-v2-{}",uuid::Uuid::new_v4()));
    std::fs::create_dir(&base).unwrap();
    let result=std::panic::catch_unwind(|| {
        for name in ["equal-length-original-mutation","missing-history","wrong-source-owner","healthy","healthy-v22","healthy-v23"] {
            let profile=base.join(name);std::env::set_var("GARDEN_PROFILE_DIR",&profile);
            tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap().block_on(async {
                let state=state(&profile);
                let before=crate::graph_service::create_graph_service(&state.app,crate::graph_service::CreateGraphInput {
                    graph_id:Some(GRAPH.into()),title:"Independent preservation consumer".into(),description:None,operation_id:None,
                }).unwrap();
                let graph_dir=crate::graph_paths::existing_graph_dir(&state.app,GRAPH).unwrap();
                let (bytes,plan)=specimen(name);
                if name.starts_with("healthy") {
                    let (status,_)=request(state.clone(),&format!("/graphs/{GRAPH}/restore-archive"),&lease("owner",Some(("owner",json!("user:foreign")))),wire(&bytes,&plan),"multipart/form-data; boundary=restore-boundary").await;
                    assert_eq!(status,StatusCode::UNAUTHORIZED);
                    assert_unclaimed_empty(&state,&graph_dir);
                }
                let (status,body)=request(state.clone(),&format!("/graphs/{GRAPH}/restore-archive"),&lease("owner",None),wire(&bytes,&plan),"multipart/form-data; boundary=restore-boundary").await;
                if !name.starts_with("healthy") {
                    if status==StatusCode::ACCEPTED {
                        let job=await_job(&state,&body).await;
                        assert!(matches!(job.status,LocalJobStatus::Failed),"{name}: {job:?}");
                    } else { assert!(status.is_client_error(),"{name}: wrong refusal class {status} {body}"); }
                    assert_unclaimed_empty(&state,&graph_dir);
                    assert!(!graph_dir.join(".migration/preservation-v2/claim.json").exists(),"{name}: claimed rejected input");
                    return;
                }
                assert_eq!(status,StatusCode::ACCEPTED,"{body}");
                let job=await_job(&state,&body).await;
                assert!(matches!(job.status,LocalJobStatus::Succeeded),"{job:?}");
                assert_eq!(job.owner_principal.as_deref(),Some(OWNER));assert_eq!(job.graph_generation,Some(1));
                if name=="healthy-v23" {
                    assert_eq!(job.detail["result_inline"]["sourceCompleteness"],json!({"scope":"persisted-storage-only","acknowledgedTail":"unknown","processMemory":"not-captured"}),"successful import must not upgrade source completeness");
                }
                let (_,after)=crate::graph_service::read_graph_record(&state.app,GRAPH).unwrap();
                assert_eq!(before.incarnation_id,after.incarnation_id);
                assert_eq!(before.created_at,after.created_at);
                if name=="healthy-v22" || name=="healthy-v23" {
                    assert_eq!(after.title,"Retained fixture graph","actual source catalogue title");
                } else {
                    assert_eq!(before.title,after.title,"v2.1 does not supply source graph title");
                }
                let custody=graph_dir.join(".migration/preservation-v2");
                assert_eq!(std::fs::read(custody.join("archive.tar.gz")).unwrap(),bytes);
                let source=members(&bytes);
                for (path,expected) in &source {
                    assert_eq!(std::fs::read(custody.join("source").join(path)).unwrap(),*expected,"source custody: {path}");
                }
                if name=="healthy-v23" {
                    let retained:Value=serde_json::from_slice(&source["source-custody/index.json"]).unwrap();
                    assert_eq!(retained["legacyDomains"]["journalTails"],"not-captured");
                    assert_eq!(retained["legacyDomains"]["processMemory"],"not-captured");
                    assert_eq!(retained["heldFiles"].as_array().unwrap().len(),3);
                    for held in retained["heldFiles"].as_array().unwrap() {
                        let path=held["member"].as_str().unwrap();
                        assert_eq!(std::fs::read(custody.join("source").join(path)).unwrap(),source[path]);
                    }
                }
                for id in ["doc-fixture-alpha","doc-fixture-beta"] {
                    let document=crate::document_service::read_document(state.app.clone(),GRAPH.into(),id.into()).unwrap();
                    assert_eq!(document.revision,1);
                    let persisted=std::fs::read(crate::paths::document_ydoc_state_path(&graph_dir,id)).unwrap();
                    assert_eq!(content_tree(&persisted),content_tree(&source[&format!("crdt/documents/{id}.yjs")]),"CRDT formatting/nesting/links changed");
                }
                assert!(crate::document_service::read_document(state.app.clone(),GRAPH.into(),"doc-fixture-deleted".into()).is_err());
                assert!(crate::document_tombstone_store::document_is_tombstoned(&graph_dir,"doc-fixture-deleted").unwrap());
                let source_deletions:Value=serde_json::from_slice(&source["deletions/index.json"]).unwrap();
                assert!(source_deletions[0]["deletionId"].is_null() && source_deletions[0]["deletedAt"].is_null());
                let original_index:Value=serde_json::from_slice(&source["originals/index.json"]).unwrap();
                let original_path=|id:&str|original_index.as_array().unwrap().iter().find(|r|r["id"]==id).unwrap()["member"].as_str().unwrap();
                for id in ["artifact-fixture-html","artifact-fixture-pdf"] {
                    let (_,actual)=crate::original_file_service::read_artifact_original_file(&state.app,GRAPH,id).unwrap();
                    assert_eq!(actual,source[original_path(id)]);
                    assert!(!crate::paths::artifacts_dir(&graph_dir).join(id).join("text-owner.json").exists(),"source custody manufactured native HTML owner");
                }
                assert!(!graph_dir.join("artifact-text-operations").exists(),"source custody manufactured native text operation receipts");
                let (_,actual)=crate::original_file_service::read_document_original_file(&state.app,GRAPH,"doc-fixture-alpha").unwrap();
                assert_eq!(actual,source[original_path("doc-fixture-alpha")]);
                let history=crate::document_history_persistence::read_document_history_store(&graph_dir,GRAPH,"doc-fixture-alpha").unwrap();
                let newest=crate::document_history_persistence::newest_snapshot_payload(&graph_dir,&history).unwrap().expect("nonempty native history");
                assert_eq!(newest.snapshot_id,history.latest_automatic_snapshot_id.clone().expect("native automatic history remains newest"),"imported older histories displaced newest native history");
                for (id,text,tier,count,instant) in [("11111111-1111-4111-8111-111111111111","Earlier moss.","20min",1,1788775200000u128),("22222222-2222-4222-8222-222222222222","Later fern.","2h",3,1788782400000u128)] {
                    let meta=history.snapshots.iter().find(|s|s.snapshot_id==id).expect("source history ID in ordinary store");
                    assert_eq!(meta.tier,tier);assert_eq!(meta.snapshot_count,count);
                    let payload=crate::document_history_persistence::read_document_snapshot_payload(&graph_dir,"doc-fixture-alpha",id).unwrap();
                    assert!(payload.tiptap_xml.contains(text));
                    assert_eq!(crate::clock::parse_timestamp(&meta.created_at),Some(instant),"native history metadata must retain the source instant");
                    assert_eq!(crate::clock::parse_timestamp(&payload.created_at),Some(instant),"native history body must retain the source instant");
                }
                let checkpoint=crate::time_travel_store::read_manifest(&graph_dir,"snap_20260907T100000Z").unwrap();
                assert_eq!(checkpoint.schema_version,1,"sparse source history must stay non-restorable");
                assert_eq!(checkpoint.documents.len(),1);
                assert_eq!(checkpoint.documents[0].document_id,"doc-fixture-alpha");
                let before_restore=std::fs::read(crate::paths::workspace_ydoc_state_path(&graph_dir)).unwrap();
                assert!(crate::time_travel_restore_service::start_restore(&state.app,state.jobs.clone(),GRAPH,"snap_20260907T100000Z",false).is_err(),"sparse source checkpoint gained destructive restore authority");
                assert_eq!(std::fs::read(crate::paths::workspace_ydoc_state_path(&graph_dir)).unwrap(),before_restore);
                let query="ASK { GRAPH ?a { <urn:demi:annotation:one> <urn:demi:fixture:label> \"Moss is not fern\"@en } GRAPH ?b { <urn:demi:annotation:two> <urn:demi:fixture:rank> \"7\"^^<http://www.w3.org/2001/XMLSchema#integer> } FILTER (?a != ?b) }";
                let answer=crate::rdf_service::run_sparql_query_service(state.app.clone(),crate::rdf_service::SparqlInput {graph_id:GRAPH.into(),query:query.into()}).unwrap();
                assert_eq!(answer.boolean,Some(true),"side RDF terms and separate graph boundaries");
                create_folder_normally(state.clone(),&lease("owner",None),"independent-after-import").await;
                let snapshot_path=crate::paths::workspace_snapshot_path(&graph_dir);
                let edited:Value=crate::storage::read_json(&snapshot_path).unwrap();
                assert!(edited["folders"].as_array().unwrap().iter().any(|f|f["id"]=="independent-after-import"));
                let (status,body)=request(state.clone(),&format!("/graphs/{GRAPH}/restore-archive"),&lease("owner",None),wire(&bytes,&plan),"multipart/form-data; boundary=restore-boundary").await;
                if status==StatusCode::ACCEPTED { let _terminal=await_job(&state,&body).await; }
                else { assert!(status.is_client_error(),"unexpected retry status {status}"); }
                let after_retry:Value=crate::storage::read_json(&snapshot_path).unwrap();
                assert_eq!(edited,after_retry,"same-operation retry erased a later destination edit");
                assert_eq!(std::fs::read(custody.join("archive.tar.gz")).unwrap(),bytes);
            });
            if name.starts_with("healthy") {
                // Previous runtime and its tasks have ended. This is a new app/
                // reader context over retained disk, not a live-room reread.
                tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap().block_on(async {
                    let cold=state(&profile);
                    let graph_dir=crate::graph_paths::existing_graph_dir(&cold.app,GRAPH).unwrap();
                    for id in ["doc-fixture-alpha","doc-fixture-beta"] {
                        assert!(crate::document_service::read_document(cold.app.clone(),GRAPH.into(),id.into()).is_ok());
                    }
                    let snapshot:Value=crate::storage::read_json(&crate::paths::workspace_snapshot_path(&graph_dir)).unwrap();
                    assert!(snapshot["folders"].as_array().unwrap().iter().any(|f|f["id"]=="independent-after-import"));
                    assert!(crate::document_tombstone_store::document_is_tombstoned(&graph_dir,"doc-fixture-deleted").unwrap());
                    let (_,original)=crate::original_file_service::read_artifact_original_file(&cold.app,GRAPH,"artifact-fixture-html").unwrap();
                    assert_eq!(original,b"<!doctype html><title>Moss</title><p>Cloud one original.</p>\n");
                });
            }
        }
    });
    std::env::remove_var("GARDEN_PROFILE_DIR");
    eprintln!("Independent consumer profiles retained: {}",base.display());
    if let Err(error)=result {std::panic::resume_unwind(error);}
}

#[test]
fn independent_v2_partial_import_cannot_overwrite_later_edit() {
    let _serial=crate::tauri_runtime::profile_env_serial().lock().unwrap_or_else(|p|p.into_inner());
    let profile=std::env::temp_dir().join(format!("garden-independent-v2-partial-{}",uuid::Uuid::new_v4()));
    std::env::set_var("GARDEN_PROFILE_DIR",&profile);
    let result=std::panic::catch_unwind(|| {
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap().block_on(async {
            let state=state(&profile);
            crate::graph_service::create_graph_service(&state.app,crate::graph_service::CreateGraphInput {
                graph_id:Some(GRAPH.into()),title:"Independent partial preservation".into(),description:None,operation_id:None,
            }).unwrap();
            let graph_dir=crate::graph_paths::existing_graph_dir(&state.app,GRAPH).unwrap();
            let (bytes,mut plan)=specimen("healthy");
            plan["operationId"]=json!("independent-v2-partial");
            crate::crdt_engine::import_archive_ops::fail_next_archive_step_for_test("independent-v2-partial",
                crate::crdt_engine::import_archive_ops::ArchiveFailurePoint::BeforeDocument);
            let owner=lease("owner",None);let route=format!("/graphs/{GRAPH}/restore-archive");
            let mime="multipart/form-data; boundary=restore-boundary";
            let (status,body)=request(state.clone(),&route,&owner,wire(&bytes,&plan),mime).await;
            assert_eq!(status,StatusCode::ACCEPTED,"{body}");
            let failed=await_job(&state,&body).await;
            assert!(matches!(failed.status,LocalJobStatus::Failed),"{failed:?}");
            let custody=graph_dir.join(".migration/preservation-v2");
            assert_eq!(std::fs::read(custody.join("archive.tar.gz")).unwrap(),bytes);
            assert!(crate::paths::workspace_ydoc_state_path(&graph_dir).exists(),"actual post-workspace cutpoint");
            assert!(!crate::paths::document_ydoc_state_path(&graph_dir,"doc-fixture-alpha").exists(),"failure occurred before first document");
            create_folder_normally(state.clone(),&owner,"independent-after-partial").await;
            let snapshot_path=crate::paths::workspace_snapshot_path(&graph_dir);
            let edited:Value=crate::storage::read_json(&snapshot_path).unwrap();
            assert!(edited["folders"].as_array().unwrap().iter().any(|f|f["id"]=="independent-after-partial"));
            let (status,body)=request(state.clone(),&route,&owner,wire(&bytes,&plan),mime).await;
            if status==StatusCode::ACCEPTED {
                let refused=await_job(&state,&body).await;
                assert!(matches!(refused.status,LocalJobStatus::Failed),"unsafe partial resume {refused:?}");
            } else {assert!(status.is_client_error(),"wrong partial-resume refusal {status} {body}");}
            let after:Value=crate::storage::read_json(&snapshot_path).unwrap();
            assert_eq!(edited,after,"partial resume erased a legitimate destination edit");
            assert_eq!(std::fs::read(custody.join("archive.tar.gz")).unwrap(),bytes);
        });
    });
    std::env::remove_var("GARDEN_PROFILE_DIR");
    eprintln!("Independent partial-import profile retained: {}",profile.display());
    if let Err(error)=result {std::panic::resume_unwind(error);}
}
