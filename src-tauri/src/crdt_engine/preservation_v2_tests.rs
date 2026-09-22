use super::*;
use flate2::{write::GzEncoder, Compression};

#[test]
fn retained_dataset_typed_mapping_and_exact_side_terms() {
    let bytes = include_bytes!("../../tests/fixtures/retained-dataset/retained-dataset-v1.tar.gz");
    let prepared = prepare(bytes, &manifest_operation(bytes)).expect("retained dataset full prepare");
    let source = member(&prepared.members, "rdf/dataset.nq").unwrap();
    let rows = prepared.dataset_partitions["partitions"].as_array().unwrap();
    assert_eq!(rows.len(), prepared.manifest["counts"]["namedGraphs"].as_u64().unwrap() as usize + 1);
    let output: BTreeSet<_> = RdfParser::from_format(RdfFormat::NQuads).for_slice(prepared.rdf.as_bytes())
        .map(|q|q.unwrap().to_string()).collect();
    let mut side_count = 0;
    for quad in RdfParser::from_format(RdfFormat::NQuads).for_slice(source) {
        let mut quad = quad.unwrap();
        let identity = match &quad.graph_name {
            GraphName::NamedNode(name)=>json!({"kind":"named","iri":name.as_str()}),
            GraphName::DefaultGraph=>json!({"kind":"default"}),
            _=>panic!("unsupported test graph"),
        };
        let row = rows.iter().find(|row|row["identity"]==identity).unwrap();
        if row["disposition"]=="main-authority" { continue; }
        quad.graph_name=NamedNode::new(row["destinationGraph"].as_str().unwrap()).unwrap().into();
        assert!(output.contains(&quad.to_string()), "exact retained side quad absent");
        side_count+=1;
    }
    assert!(side_count>=2);
    assert_eq!(prepared.dataset_partitions["archiveSha256"], archive_sha256(bytes));
    let mut destinations=BTreeSet::new();
    for row in rows { assert!(destinations.insert(row["destinationGraph"].as_str().unwrap())); }
    assert_ne!(dataset_destination("g", "archive", "default", ""), dataset_destination("g", "archive", "named", "default"));
    assert_ne!(dataset_destination("g", "archive", "named", "urn:x"), dataset_destination("other", "archive", "named", "urn:x"));
    assert_ne!(dataset_destination("g", "archive", "named", "urn:x"), dataset_destination("g", "other", "named", "urn:x"));
}

#[test]
fn retained_dataset_concessions_apply_only_to_main_authority() {
    use crate::crdt_engine::content_parity::{Concessions,
        SOURCE_ASSERTION_OUTSIDE_PROJECTION_AUTHORITY, UNRESOLVED_SOURCE_DOCUMENT_IDENTITY};

    let bytes = include_bytes!("../../tests/fixtures/retained-dataset/retained-dataset-v1.tar.gz");
    let mut prepared = prepare(bytes, &manifest_operation(bytes)).unwrap();
    let graph = prepared.manifest["source"]["graphId"].as_str().unwrap().to_string();
    let user = prepared.manifest["source"]["userId"].as_str().unwrap();
    let source = format!("urn:mnemosyne:user:{user}:graph:{graph}");
    let missing = format!("{source}:doc:missing-from-live-bodies");
    let mut rdf = String::from_utf8(member(&prepared.members, "rdf/dataset.nq").unwrap().to_vec()).unwrap();
    rdf.push_str(&format!("<{missing}> <urn:test:retained-reference> \"main\" <{source}> .\n"));
    rdf.push_str(&format!("<{missing}> <urn:test:retained-reference> \"default\" .\n"));
    let run = |parsed: &mut ParsedGraphArchive, concessions: &Concessions| {
        prepare_rdf_with_content_parity_and_partitions(&rdf, &prepared.manifest, parsed, &graph,
            &HashMap::new(), &BTreeSet::new(), Some(&archive_sha256(bytes)), concessions,
            Some(&prepared.dataset_partitions))
    };
    let error = run(&mut prepared.parsed, &Concessions::none()).unwrap_err();
    assert!(error.contains("unresolved source document identity missing-from-live-bodies"), "{error}");
    let wrong_kind = Concessions::parse(Some(&json!([SOURCE_ASSERTION_OUTSIDE_PROJECTION_AUTHORITY]))).unwrap();
    assert!(run(&mut prepared.parsed, &wrong_kind).is_err());
    let allowed = Concessions::parse(Some(&json!([UNRESOLVED_SOURCE_DOCUMENT_IDENTITY]))).unwrap();
    let (output, _, _, source_count, _, _, disposition) = run(&mut prepared.parsed, &allowed).unwrap();
    assert_eq!(source_count, prepared.manifest["counts"]["rdfQuads"].as_u64().unwrap() as usize + 2);
    let entries: Vec<_> = disposition["entries"].as_array().unwrap().iter()
        .filter(|row| row.get("contentParity").is_some()).collect();
    assert_eq!(entries.len(), 1, "only the main-authority reference needs a concession");
    assert_eq!(entries[0]["contentParity"]["concession"], UNRESOLVED_SOURCE_DOCUMENT_IDENTITY);
    assert_eq!(entries[0]["sourceArchiveSha256"], archive_sha256(bytes));
    assert_eq!(entries[0]["currentEntityExistenceAsserted"], false);
    let output: BTreeSet<_> = RdfParser::from_format(RdfFormat::NQuads).for_slice(output.as_bytes())
        .map(|quad| quad.unwrap().to_string()).collect();
    let rows = prepared.dataset_partitions["partitions"].as_array().unwrap();
    let mut side_count = 0;
    for quad in RdfParser::from_format(RdfFormat::NQuads).for_slice(rdf.as_bytes()) {
        let mut quad = quad.unwrap();
        let identity = match &quad.graph_name {
            GraphName::NamedNode(name) => json!({"kind":"named","iri":name.as_str()}),
            GraphName::DefaultGraph => json!({"kind":"default"}),
            _ => panic!("unsupported test graph"),
        };
        let row = rows.iter().find(|row| row["identity"] == identity).unwrap();
        if row["disposition"] == "main-authority" { continue; }
        quad.graph_name = NamedNode::new(row["destinationGraph"].as_str().unwrap()).unwrap().into();
        assert!(output.contains(&quad.to_string()), "side/default source terms must remain exact");
        side_count += 1;
    }
    assert!(side_count >= 3);
}

#[test]
fn retained_dataset_rejects_forged_partition_witnesses() {
    let bytes = include_bytes!("../../tests/fixtures/retained-dataset/retained-dataset-v1.tar.gz");
    let members=unpack(bytes).unwrap();
    let manifest=parse_json(member(&members,"manifest.json").unwrap()).unwrap();
    let graph=manifest["source"]["graphId"].as_str().unwrap();
    let user=manifest["source"]["userId"].as_str().unwrap();
    assert!(prepare_dataset_partitions(&members,&manifest,graph,user,&archive_sha256(bytes)).is_ok());
    for (pointer,value) in [("/schema",json!("unknown")),("/captureId",json!("wrong")),
        ("/partitions/0/quadCount",json!(100000)),("/partitions/0/identity",json!({"kind":"named","iri":"urn:default"})),
        ("/catalogueSource/sha256",json!("0".repeat(64))), ("/datasetSource/sha256",json!("0".repeat(64)))] {
        let mut changed=members.clone();
        let mut witness=parse_json(member(&members,DATASET_MEMBER).unwrap()).unwrap();
        *witness.pointer_mut(pointer).unwrap()=value;
        changed.insert(DATASET_MEMBER.into(),serde_json::to_vec(&witness).unwrap());
        assert!(prepare_dataset_partitions(&changed,&manifest,graph,user,&archive_sha256(bytes)).is_err(),"{pointer}");
    }
    let mut changed=members.clone();
    changed.remove(DATASET_MEMBER);
    let archive=pack(changed);
    assert!(prepare(&archive,&manifest_operation(&archive)).is_err(),"required partition member omission");
}

#[test]
fn retained_dataset_quoted_triples_are_explicitly_held() {
    let bytes=include_bytes!("../../tests/fixtures/retained-dataset/retained-dataset-v1.tar.gz");
    let baseline=unpack(bytes).unwrap();
    let manifest=parse_json(member(&baseline,"manifest.json").unwrap()).unwrap();
    let graph=manifest["source"]["graphId"].as_str().unwrap();
    let user=manifest["source"]["userId"].as_str().unwrap();
    for subject in ["_:nested","<urn:plain>"] {
        let mut members=baseline.clone();
        let mut rdf=member(&members,"rdf/dataset.nq").unwrap().to_vec();
        rdf.extend_from_slice(format!("<urn:s> <urn:p> <<( {subject} <urn:p> <urn:o> )>> .\n").as_bytes());
        let mut encoder=GzEncoder::new(Vec::new(),Compression::default());
        encoder.write_all(&rdf).unwrap();
        let compressed=encoder.finish().unwrap();
        let mut witness=parse_json(member(&members,DATASET_MEMBER).unwrap()).unwrap();
        let mut custody=parse_json(member(&members,"source-custody/index.json").unwrap()).unwrap();
        let source=&mut witness["datasetSource"];
        source["sha256"]=json!(archive_sha256(&compressed));
        source["byte_length"]=json!(compressed.len());
        let entry=custody["objects"].as_array_mut().unwrap().iter_mut().find(|entry|
            ["bucket","key","version_id"].iter().all(|key|entry[*key]==source[*key])).unwrap();
        entry["sha256"]=source["sha256"].clone();
        entry["byte_length"]=source["byte_length"].clone();
        entry["byteLength"]=source["byte_length"].clone();
        members.insert(entry["member"].as_str().unwrap().into(),compressed);
        witness["datasetSha256"]=json!(archive_sha256(&rdf));
        members.insert("rdf/dataset.nq".into(),rdf);
        members.insert(DATASET_MEMBER.into(),serde_json::to_vec(&witness).unwrap());
        members.insert("source-custody/index.json".into(),serde_json::to_vec(&custody).unwrap());
        let error=prepare_dataset_partitions(&members,&manifest,graph,user,&archive_sha256(bytes)).unwrap_err();
        assert_eq!(error,"preservation v2: side blank nodes or quoted triples unsupported");
    }
}

#[test]
fn remediation_c_manifest_full_prepare_and_original_reopen() {
    let row: Value = serde_json::from_slice(&fs::read(std::env::var("GARDEN_PRESERVATION_PROFILE_MANIFEST").expect("explicit manifest")).unwrap()).unwrap();
    let bytes = fs::read(std::env::var("GARDEN_PRESERVATION_RETAINED_ARCHIVE").expect("explicit archive")).unwrap();
    assert_eq!(archive_sha256(&bytes), row["archiveSha256"]);
    let prepared = prepare(&bytes, &manifest_operation(&bytes)).expect("C full prepare");
    assert_eq!(prepared.manifest["source"]["userId"], row["sourceOwner"]);
    assert_eq!(prepared.manifest["source"]["graphId"], row["graphId"]);
    assert_eq!(prepared.rdf_accounting["complete"], true);
    let output = PathBuf::from(std::env::var("GARDEN_PRESERVATION_PREPARE_OUTPUT").expect("owned output"));
    let graph_dir = output.join("original-reopen");
    fs::create_dir(&graph_dir).unwrap();
    let originals = prepared.originals.iter().map(|original| persist_original(&graph_dir, original, &prepared.members).unwrap()).collect::<Vec<_>>();
    let result = json!({"phase":"native-full-prepare-and-original-file-reopen", "signedImport":false,
        "sourceUserId":row["sourceOwner"],"sourceGraphId":row["graphId"],"archiveSha256":row["archiveSha256"],
        "counts":prepared.manifest["counts"], "originalPayloads":originals,
        "rdfPreservationAccounting":prepared.rdf_accounting, "productionRequests":0});
    fs::write(output.join("result.json"),serde_json::to_vec_pretty(&result).unwrap()).unwrap();
}

#[test]
fn original_unsafe_names_are_inert_data_not_storage_paths() {
    assert_eq!(original_storage_filename("normal.pdf").unwrap(),"normal.pdf");
    let mut mapped=BTreeSet::new();
    for value in ["../outside","/absolute","a/b.pdf","a\\b.pdf","line\r\nheader",
        "manifest.json","MANIFEST.JSON",".",".."," padded.pdf "] {
        let name=original_storage_filename(value).unwrap();
        assert_eq!(name,format!("source-{}.bin",archive_sha256(value.as_bytes())));
        assert!(crate::original_file_manifest_store::original_manifest_file_path(Path::new("owned"),&name).is_ok());
        assert!(mapped.insert(name));
    }
    assert!(original_storage_filename("").is_err());
    assert!(original_storage_filename(&"x".repeat(65537)).is_err());
}

#[test]
fn original_opaque_filename_reopens_exact_bytes_and_source_name_testimony() {
    let directory=std::env::temp_dir().join(format!("original-name-{}",uuid::Uuid::new_v4()));
    fs::create_dir_all(&directory).unwrap();
    let source_name="course/week 1.pdf";
    let storage_name=original_storage_filename(source_name).unwrap();
    let original=Original {kind:"document".into(),id:"doc".into(),filename:storage_name.clone(),
        mime:"application/pdf".into(),member:"originals/owned.bin".into(),
        provenance:json!({"sourceFilename":source_name,"storageFilename":storage_name,
            "filenameDisposition":"opaque-storage-key-exact-source-filename-retained-v1"})};
    let members=BTreeMap::from([("originals/owned.bin".into(),b"exact retained original".to_vec())]);
    let proof=persist_original(&directory,&original,&members).unwrap();
    let (manifest,bytes)=crate::original_file_storage::read_original_file_from_dir(&directory.join("documents/doc/original")).unwrap();
    assert_eq!(bytes,members["originals/owned.bin"]);
    assert_eq!(manifest.filename,storage_name);
    assert_eq!(manifest.source_filename.as_deref(),Some(source_name));
    assert_eq!(proof["provenance"]["sourceFilename"],source_name);
    assert!(!directory.join("documents/doc/original/course").exists());
}

#[test]
fn original_artifact_namespace_rewrite_requires_exact_owned_and_native_keys() {
    use oxigraph::model::{Quad,NamedNode,Literal,GraphName};
    let subject=NamedNode::new("urn:mnemosyne:local:graph:g:artifact:a").unwrap();
    let predicate=NamedNode::new("http://mnemosyne.dev/doc#storageKey").unwrap();
    let raw="users/owner/graphs/g/artifacts/a/original/input.bin";
    let native="users/default/graphs/g/artifacts/a/original/input.bin";
    let quad=Quad::new(subject.clone(),predicate.clone(),Literal::new_simple_literal(raw),GraphName::DefaultGraph);
    let entry=json!({"sourceUserId":"owner","sourceGraphId":"g","subject":subject.as_str(),
        "original":{"ownerKind":"artifact","id":"a"},
        "source":{"lexical":raw,"datatype":"http://www.w3.org/2001/XMLSchema#string"},
        "native":{"lexical":native,"datatype":"http://www.w3.org/2001/XMLSchema#string"},
        "authoritativeProjectionField":{"root":"artifacts","entityId":"a","key":"storageKey","presence":"present","value":raw}});
    let projected:std::collections::HashSet<String>=[format!("{subject} {predicate} {}",Literal::new_simple_literal(native))].into_iter().collect();
    assert!(legacy_owned_artifact_projection(&entry,&quad,&projected));
    assert!(!legacy_owned_artifact_projection(&entry,&quad,&Default::default()));
    let mut duplicate=projected.clone();duplicate.insert(format!("{subject} {predicate} {}",Literal::new_simple_literal(raw)));
    assert!(!legacy_owned_artifact_projection(&entry,&quad,&duplicate));
    for (pointer,value) in [("/sourceUserId",json!("other")),("/sourceGraphId",json!("other")),
        ("/original/ownerKind",json!("document")),("/original/id",json!("other")),
        ("/native/lexical",json!(raw)),("/source/datatype",json!("http://www.w3.org/2001/XMLSchema#anyURI")),
        ("/authoritativeProjectionField/presence",json!("absent")),("/authoritativeProjectionField/value",json!("other"))] {
        let mut bad=entry.clone();*bad.pointer_mut(pointer).unwrap()=value;
        assert!(!legacy_owned_artifact_projection(&bad,&quad,&projected),"{pointer}");
    }
}

#[test]
fn original_document_and_artifact_size_evidence_uses_actual_ymap_field() {
    for field in ["sf_sizeBytes", "sizeBytes"] {
        let workspace = yrs::Doc::new();
        let row = workspace.get_or_insert_map("owner");
        let evidence = |presence| json!({"schema":"cloud1-original-metadata-evidence.v1",
            "rule":"verified-object-size-source-absent-or-null-v1", "basis":"captured-workspace-yjs",
            "field":"sizeBytes","presence":presence,"sourceValue":null});
        assert!(workspace_original_size_evidence(&workspace,&row,field,Some(&evidence("absent"))).unwrap());
        assert!(workspace_original_size_evidence(&workspace,&row,field,Some(&evidence("null"))).is_err());
        row.insert(&mut workspace.transact_mut(),field,Any::Null);
        assert!(workspace_original_size_evidence(&workspace,&row,field,Some(&evidence("null"))).unwrap());
        assert!(workspace_original_size_evidence(&workspace,&row,field,Some(&evidence("absent"))).is_err());
        row.insert(&mut workspace.transact_mut(),field,Any::Number(1.0));
        assert!(workspace_original_size_evidence(&workspace,&row,field,Some(&evidence("null"))).is_err());
    }
}

#[test]
fn original_size_absence_evidence_is_exact_not_a_numeric_waiver() {
    let evidence = |basis: &str, field: &str, presence: &str| json!({
        "schema":"cloud1-original-metadata-evidence.v1",
        "rule":"verified-object-size-source-absent-or-null-v1",
        "basis":basis,"field":field,"presence":presence,"sourceValue":null
    });
    for (basis,field,presence) in [
        ("captured-workspace-yjs","sizeBytes","absent"),
        ("captured-workspace-yjs","sizeBytes","null"),
        ("authored-rdf-v1","sourceContentSize","absent"),
    ] {
        let value=evidence(basis,field,presence);
        assert!(original_size_evidence(Some(&value),basis,field,presence).unwrap());
        assert!(original_size_evidence(Some(&value),basis,field,"present").is_err());
        for key in ["schema","rule","basis","field","presence","sourceValue"] {
            let mut bad=value.clone();bad[key]=json!("wrong");
            assert!(original_size_evidence(Some(&bad),basis,field,presence).is_err(),"{key}");
        }
        let mut extra=value.clone();extra["trusted"]=json!(true);
        assert!(original_size_evidence(Some(&extra),basis,field,presence).is_err());
    }
    assert!(!original_size_evidence(None,"captured-workspace-yjs","sizeBytes","present").unwrap());
    assert!(original_size_evidence(Some(&evidence("authored-rdf-v1","sourceContentSize","null")),
        "authored-rdf-v1","sourceContentSize","null").is_err());
}

#[test]
fn rdf_original_absent_size_still_requires_exact_other_ownership_fields() {
    let source = |extra: &str| format!(concat!(
        "<urn:mnemosyne:user:owner:graph:graph:doc:doc> <http://mnemosyne.dev/doc#sourceStorageKey> \"users/owner/graphs/graph/file\" <urn:mnemosyne:user:owner:graph:graph> .\n",
        "<urn:mnemosyne:user:owner:graph:graph:doc:doc> <http://mnemosyne.dev/doc#sourceOriginalFilename> \"file.pdf\" <urn:mnemosyne:user:owner:graph:graph> .\n",
        "<urn:mnemosyne:user:owner:graph:graph:doc:doc> <http://mnemosyne.dev/doc#sourceMimeType> \"application/pdf\" <urn:mnemosyne:user:owner:graph:graph> .\n",
        "<urn:mnemosyne:user:owner:graph:graph:doc:doc> <http://mnemosyne.dev/doc#sourceFileType> \"pdf\" <urn:mnemosyne:user:owner:graph:graph> .\n{}"),extra);
    let expected=RdfOriginal {document_id:"doc",storage_key:"users/owner/graphs/graph/file",filename:"file.pdf",
        mime:"application/pdf",file_type:"pdf",byte_length:123};
    let evidence=json!({"schema":"cloud1-original-metadata-evidence.v1",
        "rule":"verified-object-size-source-absent-or-null-v1","basis":"authored-rdf-v1",
        "field":"sourceContentSize","presence":"absent","sourceValue":null});
    assert!(!authored_rdf_original_matches(&source(""),"owner","graph",&expected).unwrap());
    assert!(authored_rdf_original_matches_with_evidence(&source(""),"owner","graph",&expected,Some(&evidence)).unwrap());
    for (owner,graph) in [("wrong","graph"),("owner","wrong")] {
        assert!(!authored_rdf_original_matches_with_evidence(&source(""),owner,graph,&expected,Some(&evidence)).unwrap());
    }
    let duplicate=source("")+&source("");
    assert!(!authored_rdf_original_matches_with_evidence(&duplicate,"owner","graph",&expected,Some(&evidence)).unwrap());
    for size in ["0","123","124"] {
        let row=format!("<urn:mnemosyne:user:owner:graph:graph:doc:doc> <http://mnemosyne.dev/doc#sourceContentSize> \"{size}\"^^<http://www.w3.org/2001/XMLSchema#integer> <urn:mnemosyne:user:owner:graph:graph> .\n");
        assert!(authored_rdf_original_matches_with_evidence(&source(&row),"owner","graph",&expected,Some(&evidence)).is_err());
    }
}

fn fixture(v22: bool) -> Vec<u8> {
    let path = if v22 {
        PathBuf::from(
            std::env::var("GARDEN_PRESERVATION_CAPTURE_ARCHIVE")
                .expect("actual capture archive required"),
        )
    } else {
        PathBuf::from(
            std::env::var("GARDEN_PRESERVATION_EXAMINER_INPUTS")
                .expect("actual producer corpus required"),
        )
        .join("healthy.tar.gz")
    };
    fs::read(path).unwrap()
}
fn null_fixture() -> Vec<u8> {
    fs::read(
        std::env::var("GARDEN_PRESERVATION_NULL_ARCHIVE")
            .expect("explicit literal-null v2.3 archive required"),
    )
    .unwrap()
}
fn raw_only_null_fixture() -> Vec<u8> {
    let mut members = unpack(&null_fixture()).unwrap();
    let mut manifest = parse_json(&members["manifest.json"]).unwrap();
    let mut custody = parse_json(&members["source-custody/index.json"]).unwrap();
    let data = b"retained raw-only original".to_vec();
    let data_len = data.len();
    let digest = archive_sha256(&data);
    let bucket = "capture-crdt-bucket";
    let key = "users/fixture-cloud1-owner-20260908/graphs/demi-cross-format-fixture-20260908/orphan-original.bin";
    let identity = json!([bucket, key, "null"]);
    let identity_hash = archive_sha256(&json_bytes(&identity).unwrap());
    let path = format!("source-custody/objects/{identity_hash}.bin");
    custody["objects"].as_array_mut().unwrap().push(json!({
        "bucket":bucket,"key":key,"version_id":"null","member":path,
        "sha256":digest,"byteLength":data_len,"byte_length":data_len,
        "latest":true,"metadata":{}
    }));
    custody["providerVersionSelection"]["selectedNullVersionCount"] = json!(2);
    let inventory = archive_sha256(b"raw-only-null-fixture-inventory");
    custody["inventorySha256"] = json!(inventory);
    custody["nullVersionReadback"]["firstInventorySha256"] = json!(inventory);
    custody["nullVersionReadback"]["secondInventorySha256"] = json!(inventory);
    members.insert(
        "source-custody/index.json".into(),
        json_bytes(&custody).unwrap(),
    );
    members.insert(path.clone(), data);

    let generated_source = json!({"bucket":bucket,
        "key":format!("users/fixture-cloud1-owner-20260908/graphs/demi-cross-format-fixture-20260908/preservation-captures/literal-null/{path}"),
        "version_id":format!("sha256-{digest}"),"sha256":digest,"byte_length":data_len});
    manifest["classes"]["source-custody"]
        .as_array_mut()
        .unwrap()
        .push(json!(path));
    manifest["sourceObjects"][&path] = generated_source.clone();
    let class_paths = manifest["classes"]["source-custody"].clone();
    let coverage = archive_sha256(&json_bytes(&class_paths).unwrap());
    manifest["source"]["capture"]["coverage"]["source-custody"] = json!(coverage);
    let mut descriptor = parse_json(
        manifest["captureDescriptorCanonicalJson"]
            .as_str()
            .unwrap()
            .as_bytes(),
    )
    .unwrap();
    descriptor["members"].as_array_mut().unwrap().push(json!({
        "path":path,"class_name":"source-custody","source":generated_source
    }));
    let coverage_pair = descriptor["coverage"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|pair| pair[0] == "source-custody")
        .unwrap();
    coverage_pair[1] = json!(coverage);
    manifest["captureDescriptorCanonicalJson"] =
        json!(String::from_utf8(json_bytes(&descriptor).unwrap()).unwrap());
    members.insert("manifest.json".into(), json_bytes(&manifest).unwrap());
    reseal(&mut members);
    pack(members)
}
fn custody_only_null_fixture() -> Vec<u8> {
    let mut members = unpack(&raw_only_null_fixture()).unwrap();
    let mut manifest = parse_json(&members["manifest.json"]).unwrap();
    let source = &mut manifest["sourceObjects"]["crdt/workspace.yjs"];
    source["version_id"] = json!(format!("sha256-{}", source["sha256"].as_str().unwrap()));
    manifest["source"]["capture"]["snapshotBoundaries"][0]["source"] = source.clone();
    let mut descriptor = parse_json(
        manifest["captureDescriptorCanonicalJson"]
            .as_str()
            .unwrap()
            .as_bytes(),
    )
    .unwrap();
    descriptor["snapshot_boundaries"] = manifest["source"]["capture"]["snapshotBoundaries"].clone();
    manifest["captureDescriptorCanonicalJson"] =
        json!(String::from_utf8(json_bytes(&descriptor).unwrap()).unwrap());
    members.insert("manifest.json".into(), json_bytes(&manifest).unwrap());
    reseal(&mut members);
    pack(members)
}
fn operation(bytes: &[u8]) -> CrdtOperation {
    CrdtOperation {
        operation_id: "pure-preservation-check".into(),
        kind: "graph.restoreArchive".into(),
        graph_id: "demi-cross-format-fixture-20260908".into(),
        document_id: None,
        enqueue_timestamp: "2026-09-11T00:00:00Z".into(),
        payload: json!({"sourceGraphId":"demi-cross-format-fixture-20260908","sourceUserId":"fixture-cloud1-owner-20260908",
            "archiveSha256":archive_sha256(bytes),"planDigest":"a".repeat(64),"formatVersion":2,
            "targetGeneration":1,"expectedDocumentCount":2,"expectedRdfTripleCount":11}),
    }
}
fn manifest_operation(bytes: &[u8]) -> CrdtOperation {
    let members = unpack(bytes).unwrap();
    let manifest = parse_json(&members["manifest.json"]).unwrap();
    let graph = manifest["source"]["graphId"].as_str().unwrap();
    CrdtOperation {
        operation_id: "pure-preservation-real-null-check".into(),
        kind: "graph.restoreArchive".into(),
        graph_id: graph.into(),
        document_id: None,
        enqueue_timestamp: "2026-09-11T00:00:00Z".into(),
        payload: json!({
            "sourceGraphId":graph,
            "sourceUserId":manifest["source"]["userId"],
            "archiveSha256":archive_sha256(bytes),
            "planDigest":"b".repeat(64),
            "formatVersion":2,
            "targetGeneration":1,
            "expectedDocumentCount":manifest["counts"]["documents"],
            "expectedRdfTripleCount":manifest["counts"]["rdfQuads"]
        }),
    }
}
fn actual_orphan_archive() -> Vec<u8> {
    let bytes = fs::read(
        std::env::var("GARDEN_PRESERVATION_REAL_NULL_ARCHIVE")
            .expect("actual saved-state archive required"),
    )
    .unwrap();
    assert_eq!(
        archive_sha256(&bytes),
        "4bc785e016076ef262d6171a90a3b4a4ec3df1b0c36e88fdf67d23b62c49e015",
        "actual archive pin changed"
    );
    bytes
}

fn actual_history_archive() -> Vec<u8> {
    let bytes = fs::read(
        std::env::var("GARDEN_PRESERVATION_HISTORY_ARCHIVE")
            .expect("actual legacy-history archive required"),
    )
    .unwrap();
    assert_eq!(
        archive_sha256(&bytes),
        "fa57194f3925ecfeab0fc8dcf230fa600335b4e921f88c27ea2ef29fa2c028b2",
        "actual history archive pin changed"
    );
    bytes
}
fn actual_history_archive_08() -> Vec<u8> {
    let bytes = fs::read(
        std::env::var("GARDEN_PRESERVATION_HISTORY_ARCHIVE_08")
            .expect("actual candidate-08 history archive required"),
    )
    .unwrap();
    assert_eq!(
        archive_sha256(&bytes),
        "3d092a7b9609fde7f5165cf497e236d2ab6d440c8b2404c80abbc32281fc212c",
        "actual candidate-08 archive pin changed"
    );
    bytes
}
fn actual_candidate23_archive() -> Vec<u8> {
    let bytes = fs::read(
        std::env::var("GARDEN_PRESERVATION_CANDIDATE23_ARCHIVE")
            .expect("actual candidate-23 archive required"),
    )
    .unwrap();
    assert_eq!(
        archive_sha256(&bytes),
        "ecca0ebf97e31679420fea82eb5317933046bd23f1e7d968a6756d3f8851302f",
        "actual candidate-23 archive pin changed"
    );
    bytes
}

#[test]
fn actual_legacy_history_export_is_native_encoded_with_raw_custody() {
    let bytes = actual_history_archive();
    let members = unpack(&bytes).unwrap();
    let manifest = parse_json(&members["manifest.json"]).unwrap();
    let history = prepare_history(
        &members,
        &manifest,
        manifest["source"]["graphId"].as_str().unwrap(),
        manifest["source"]["userId"].as_str().unwrap(),
    )
    .unwrap();
    let index = parse_json(&members["history/documents/index.json"]).unwrap();
    let mut repaired = 0;
    for payload in &history {
        let row = index
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["snapshot_id"] == payload.payload.snapshot_id)
            .unwrap();
        let source = parse_json(&members[row["member"].as_str().unwrap()]).unwrap();
        let source_xml = source["tiptap_xml"].as_str().unwrap();
        if payload.payload.tiptap_xml != source_xml {
            repaired += 1;
            assert_eq!(
                payload.payload.tiptap_xml,
                legacy_history_export_xml(source_xml,source["blocks"].as_array().unwrap()).unwrap()
            );
        }
        let parsed = crate::crdt_engine::content_parse::parse_write_content_for_operation(
            &payload.payload.tiptap_xml,
            Some("xml"),
            &payload.payload.snapshot_id,
        )
        .unwrap();
        assert!(parsed.warnings.is_empty());
    }
    assert_eq!(history.len(), 6);
    assert!(repaired >= 4);
    assert_eq!(
        archive_sha256(&bytes),
        "fa57194f3925ecfeab0fc8dcf230fa600335b4e921f88c27ea2ef29fa2c028b2"
    );
}

#[test]
fn legacy_ampersand_repair_preserves_valid_entities_and_unicode() {
    assert_eq!(
        escape_legacy_bare_xml_ampersands("<p>α & β &amp; &#38; &#x26; &bogus;</p>"),
        "<p>α &amp; β &amp; &#38; &#x26; &amp;bogus;</p>"
    );
}

#[test]
fn population_manifest_full_prepare() {
    let row:Value=serde_json::from_slice(&fs::read(std::env::var("GARDEN_PRESERVATION_PROFILE_MANIFEST").expect("explicit population manifest required")).unwrap()).unwrap();
    let bytes=fs::read(std::env::var("GARDEN_PRESERVATION_RETAINED_ARCHIVE").expect("explicit retained archive required")).unwrap();
    assert_eq!(archive_sha256(&bytes),row["archiveSha256"]);
    let operation=manifest_operation(&bytes);
    let started=std::time::Instant::now();
    eprintln!("POPULATION_PHASE prepare-start 0");
    let prepared=prepare(&bytes,&operation).unwrap_or_else(|error|panic!("population native prepare failed: {error}"));
    eprintln!("POPULATION_PHASE prepare-complete {}",started.elapsed().as_millis());
    assert_eq!(prepared.manifest["source"]["userId"],row["sourceOwner"]);
    assert_eq!(prepared.manifest["source"]["graphId"],row["graphId"]);
    assert_eq!(prepared.manifest["counts"]["documents"],row["documents"]);
    assert_eq!(prepared.manifest["counts"]["rdfQuads"],row["rdfQuads"]);
    assert_eq!(prepared.manifest["counts"]["documentSnapshots"],row["histories"]);
    assert_eq!(prepared.rdf_accounting["complete"],true);
    let result=json!({"phase":"native-full-prepare-only","archiveSha256":archive_sha256(&bytes),
        "sourceUserId":prepared.manifest["source"]["userId"],"sourceGraphId":prepared.manifest["source"]["graphId"],
        "counts":prepared.manifest["counts"],"presentDocumentBodyCount":prepared.parsed.documents.len(),
        "unavailableDocumentBodyCount":prepared.unavailable_bodies.len(),"rdfPreservationAccounting":prepared.rdf_accounting,
        "sourceDerivedAssertionDisposition":prepared.derived_disposition,
        "legacyProjectionNormalization":prepared.timestamp_normalization,
        "signedImport":false,"productionRequests":0});
    fs::write(std::env::temp_dir().join("prepared-rdf.nq"),&prepared.rdf).unwrap();
    fs::write(std::env::temp_dir().join("prepare-result.json"),serde_json::to_vec_pretty(&result).unwrap()).unwrap();
}

#[test]
fn actual_wf_emit_lab_rich_archive_full_prepare() {
    let bytes = actual_history_archive_08();
    let members = unpack(&bytes).unwrap();
    let manifest = parse_json(&members["manifest.json"]).unwrap();
    let prepared = prepare(&bytes, &manifest_operation(&bytes)).unwrap();
    assert_eq!(prepared.members, members);
    let dispositions=prepared.derived_disposition["entries"].as_array().unwrap();
    assert_eq!(dispositions.iter().filter(|row|row["predicate"]=="http://mnemosyne.dev/doc#order").count(),31);
    assert_eq!(dispositions.iter().filter(|row|row["predicate"]=="http://mnemosyne.dev/doc#lastAccessedAt").count(),24);
    emit_actual_prepared_fixture(&prepared,&bytes,"wf-emit-lab");
    assert_eq!(
        prepared.history.len() as u64,
        manifest["counts"]["documentSnapshots"].as_u64().unwrap()
    );
    assert!(prepared.history.iter().any(|item| {
        let row = manifest["classes"]["document-history"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .find_map(|path| {
                let body = parse_json(&members[path]).ok()?;
                (body["snapshot_id"] == item.payload.snapshot_id).then_some(body)
            })
            .unwrap();
        item.payload.tiptap_xml != row["tiptap_xml"]
    }));
}

#[test]
fn actual_choreograph_2_rich_archive_full_prepare() {
    let bytes = actual_candidate23_archive();
    let members = unpack(&bytes).unwrap();
    let prepared = prepare(&bytes, &manifest_operation(&bytes)).unwrap();
    assert!(!prepared.timestamp_normalization["entries"].as_array().unwrap().is_empty());
    assert_eq!(prepared.members, members);
    assert_eq!(prepared.parsed.documents.len(), 62);
    assert_eq!(prepared.history.len(), 6);
    assert_eq!(prepared.originals.len(), 1);
    assert_eq!(prepared.graph_history.len(), 1);
    assert_eq!(prepared.graph_history[0].manifest.schema_version, 1);
    emit_actual_prepared_fixture(&prepared,&bytes,"choreograph-2");
    let original = &prepared.originals[0];
    assert_eq!(original.kind, "document");
    assert_eq!(original.id, "34d72f96-7368-4ee8-9d3c-5c09af05deaa");
    assert_eq!(original.filename, "Cantrip SPEC.md");
    assert_eq!(original.mime, "text/markdown");
    let body = &prepared.members[&original.member];
    assert_eq!(body.len(), 73_127);
    assert_eq!(
        archive_sha256(body),
        "9550be9c7620eba8c4d2977e99aedfd75d592b27451a51f274c09eda3175ab9e"
    );
}

#[test]
fn rdf_original_ownership_mismatch_and_ambiguity_are_refused() {
    for mutation in ["basis", "key", "filename", "mime", "file-type", "size", "duplicate", "workspace-file-type", "workspace-source-file"] {
        let mut members = unpack(&actual_candidate23_archive()).unwrap();
        if mutation.starts_with("workspace-") {
            let index=parse_json(&members["originals/index.json"]).unwrap();
            let workspace=parsed_doc(&members["crdt/workspace.yjs"],"negative original workspace").unwrap();
            let documents=roots(&workspace,"documents").unwrap();
            let row=&documents[index[0]["id"].as_str().unwrap()];
            row.insert(&mut workspace.transact_mut(),if mutation=="workspace-file-type" {"sf_fileType"} else {"sourceFile"},"must-not-overwrite");
            members.insert("crdt/workspace.yjs".into(),encode_full_state(&workspace));
        } else if mutation == "duplicate" {
            let rdf = String::from_utf8(members["rdf/dataset.nq"].clone()).unwrap();
            let line = rdf
                .lines()
                .find(|line| {
                    line.contains("34d72f96-7368-4ee8-9d3c-5c09af05deaa")
                        && line.contains("sourceStorageKey")
                })
                .unwrap();
            members.insert(
                "rdf/dataset.nq".into(),
                format!("{rdf}{line}\n").into_bytes(),
            );
        } else {
            let mut index = parse_json(&members["originals/index.json"]).unwrap();
            let entry = &mut index[0];
            match mutation {
                "basis" => entry["ownershipBasis"] = json!("unknown"),
                "key" => entry["sourceStorageKey"] = json!("other"),
                "filename" => entry["filename"] = json!("other.md"),
                "mime" => entry["mimeType"] = json!("application/octet-stream"),
                "file-type" => entry["fileType"] = json!("txt"),
                _ => entry["byteLength"] = json!(0),
            }
            members.insert("originals/index.json".into(), json_bytes(&index).unwrap());
        }
        reseal(&mut members);
        let changed = pack(members);
        assert!(prepare(&changed, &manifest_operation(&changed)).is_err(), "{mutation}");
    }
}

#[test]
fn actual_rdf_original_ownership_join_is_exact() {
    let bytes = actual_history_archive();
    let members = unpack(&bytes).unwrap();
    let rdf = std::str::from_utf8(&members["rdf/dataset.nq"]).unwrap();
    let expected = RdfOriginal {
        document_id: "34d72f96-7368-4ee8-9d3c-5c09af05deaa",
        storage_key: "users/e9e949fe-0091-7015-0ab8-10bf259084ab/graphs/choreograph-2/artifacts/9a1f4ba2-cdf9-4ff4-b82a-ef9c3654a9c8/original/Cantrip SPEC.md",
        filename: "Cantrip SPEC.md",
        mime: "text/markdown",
        file_type: "md",
        byte_length: 73_127,
    };
    assert!(authored_rdf_original_matches(
        rdf,
        "e9e949fe-0091-7015-0ab8-10bf259084ab",
        "choreograph-2",
        &expected,
    )
    .unwrap());
    let duplicate = rdf
        .lines()
        .find(|line| {
            line.contains("34d72f96-7368-4ee8-9d3c-5c09af05deaa")
                && line.contains("sourceStorageKey")
        })
        .unwrap();
    assert!(!authored_rdf_original_matches(
        &format!("{rdf}{duplicate}\n"),
        "e9e949fe-0091-7015-0ab8-10bf259084ab",
        "choreograph-2",
        &expected,
    )
    .unwrap());
    for changed in [
        RdfOriginal { document_id: "other", ..expected },
        RdfOriginal { storage_key: "other", ..expected },
        RdfOriginal { filename: "other", ..expected },
        RdfOriginal { mime: "other", ..expected },
        RdfOriginal { file_type: "other", ..expected },
        RdfOriginal { byte_length: 0, ..expected },
    ] {
        assert!(!authored_rdf_original_matches(
            rdf,
            "e9e949fe-0091-7015-0ab8-10bf259084ab",
            "choreograph-2",
            &changed,
        )
        .unwrap());
    }
}

#[test]
fn legacy_integral_size_lexical_is_exact_and_bounded() {
    assert_eq!(legacy_nonnegative_integral_lexical("73127.0"), Some(73_127));
    assert_eq!(legacy_nonnegative_integral_lexical("73127.000"), Some(73_127));
    assert_eq!(legacy_nonnegative_integral_lexical("73127"), Some(73_127));
    for refused in ["", ".0", "-1", "+1", "1.", "1.2", "1e0", "NaN"] {
        assert_eq!(legacy_nonnegative_integral_lexical(refused), None);
    }
}

#[test]
fn actual_history_payload_witness_matches_persisted_interpretation_bytes() {
    let bytes=actual_candidate23_archive();
    let members=unpack(&bytes).unwrap();
    let manifest=parse_json(&members["manifest.json"]).unwrap();
    let history=prepare_history(&members,&manifest,manifest["source"]["graphId"].as_str().unwrap(),
        manifest["source"]["userId"].as_str().unwrap()).unwrap();
    let directory=std::env::temp_dir().join(format!("preservation-history-witness-{}",uuid::Uuid::new_v4()));
    fs::create_dir(&directory).unwrap();
    let witnesses=persist_history(&directory,&history).unwrap();
    assert_eq!(witnesses.len(),history.len());
    for witness in &witnesses {
        let data=fs::read(directory.join(witness["path"].as_str().unwrap())).unwrap();
        assert_eq!(witness["sha256"],archive_sha256(&data));
        assert_eq!(witness["byteLength"],data.len());
        let source=history.iter().find(|h| h.meta.snapshot_id==witness["snapshotId"].as_str().unwrap()).unwrap();
        assert_eq!(data,json_bytes(&source.payload).unwrap());
    }
    assert!(persist_history(&directory,&history).is_err());
    // Synthetic destination only; raw archive and source custody untouched.
    fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn legacy_history_export_interpretation_preserves_literal_angles_entities_and_mark_policy() {
    let text = "<placeholder> &amp; <bold>x</bold>";
    assert_eq!(encode_legacy_history_text(text,false).unwrap(),"&lt;placeholder&gt; &amp;amp; <bold>x</bold>");
    assert_eq!(encode_legacy_history_text(text,true).unwrap(),"&lt;placeholder&gt; &amp;amp; &lt;bold&gt;x&lt;/bold&gt;");
    // The export is ambiguous: literal mark-looking source and actual marks
    // serialize identically. We name an interpretation, not infer provenance.
    let xml = format!("<paragraph data-block-id=\"p\">{text}</paragraph>");
    let row = json!({"id":"p","type":"paragraph","text":text,"parent_id":null,"index":0,"order":0,"collapsed":false});
    let encoded = legacy_history_export_xml(&xml,&[row.clone()]).unwrap();
    let parsed = crate::crdt_engine::content_parse::parse_write_content_for_operation(&encoded,Some("xml"),"test").unwrap();
    assert!(parsed.warnings.is_empty());
    validate_legacy_history_blocks(&encoded,&[row.clone()],&parsed.tiptap_json).unwrap();
    let projection = crate::crdt_engine::projection::materialize_tiptap_json(&parsed.tiptap_json,"test");
    assert_eq!(projection.blocks_json[0]["content"],"<placeholder> &amp; x");
    assert!(legacy_history_export_xml(&format!("{xml}{xml}"),&[row.clone()]).is_err());
    let nested = "<paragraph data-block-id=\"fake\">x</paragraph>";
    let outer = format!("<paragraph data-block-id=\"outer\">{nested}</paragraph>");
    assert!(legacy_history_export_xml(&outer,&[
        json!({"id":"outer","type":"paragraph","text":nested}),
        json!({"id":"fake","type":"paragraph","text":"x"}),
    ]).is_err());
    let mut wrong = row; wrong["text"] = json!("changed");
    assert!(legacy_history_export_xml(&xml,&[wrong]).is_err());
    assert_ne!(legacy_decimal_attribute("9007199254740992"),legacy_decimal_attribute("9007199254740993"));
    assert_eq!(legacy_decimal_attribute("1.000"),Some("1"));
    assert_eq!(legacy_decimal_attribute("1e0"),None);
}

#[test]
fn captured_numeric_epoch_conversion_is_exact_decimal_not_float_equality() {
    use oxigraph::model::{Quad,NamedNode,Literal,GraphName};
    let subject=NamedNode::new("urn:synthetic:document").unwrap();
    let predicate=NamedNode::new("http://mnemosyne.dev/doc#createdAt").unwrap();
    let datatype=NamedNode::new("http://www.w3.org/2001/XMLSchema#dateTime").unwrap();
    let quad=Quad::new(subject.clone(),predicate.clone(),Literal::new_typed_literal("1773944412027.0",datatype.clone()),GraphName::DefaultGraph);
    let projection=[format!("{subject} {predicate} {}",Literal::new_simple_literal("1773944412027"))].into_iter().collect();
    let authority=HashMap::from([(subject.as_str().to_string(),json!({"createdAt":{"root":"documents","entityId":"document","key":"createdAt","presence":"present","value":1773944412027.0}}))]);
    assert!(legacy_epoch_decimal_normalization(&quad,&projection,&authority).is_some());
    let mut string_authority=authority.clone();
    string_authority.get_mut(subject.as_str()).unwrap()["createdAt"]["value"]=json!("1773944412027");
    assert!(legacy_epoch_decimal_normalization(&quad,&projection,&string_authority).is_some());
    for value in [Value::Null,json!(1773944412028.0),json!("1773944412028")] {
        let mut wrong=authority.clone();wrong.get_mut(subject.as_str()).unwrap()["createdAt"]["value"]=value;
        assert!(legacy_epoch_decimal_normalization(&quad,&projection,&wrong).is_none());
    }
    let wrong=Quad::new(subject,predicate,Literal::new_typed_literal("1773944412027.0001",datatype),GraphName::DefaultGraph);
    assert!(legacy_epoch_decimal_normalization(&wrong,&projection,&authority).is_none());
}

fn emit_actual_prepared_fixture(prepared:&Prepared, archive:&[u8], label:&str) {
    let Ok(root)=std::env::var("GARDEN_PRESERVATION_PREPARE_OUTPUT") else { return; };
    let directory=std::path::Path::new(&root).join(label);
    fs::create_dir(&directory).unwrap();
    let native=directory.join("native");fs::create_dir(&native).unwrap();
    let histories=persist_history(&native,&prepared.history).unwrap();
    let originals=prepared.originals.iter().map(|original|persist_original(&native,original,&prepared.members).unwrap()).collect::<Vec<_>>();
    for original in &prepared.originals { assert!(persist_original(&native,original,&prepared.members).is_err()); }
    let result=json!({"qualification":"effect-free-prepare-and-private-file-persistence-not-running-cell",
        "archiveSha256":archive_sha256(archive),"sourceUserId":prepared.manifest["source"]["userId"],"sourceGraphId":prepared.manifest["source"]["graphId"],
        "counts":prepared.manifest["counts"],"rdfNamedGraphs":prepared.graph_mapping,"regeneratedRdfStatementCount":prepared.regenerated,
        "legacyProjectionNormalization":prepared.timestamp_normalization,"sourceDerivedAssertionDisposition":prepared.derived_disposition,
        "rdfPreservationAccounting":prepared.rdf_accounting,"rdfMappedTestimonyFullSet":prepared.rdf_accounting["rdfMappedTestimonyFullSet"],
        "documentHistoryPayloads":histories,"originalPayloads":originals,
        "unavailableDocumentHistory":prepared.unavailable_history,"documentHistoryAvailability":prepared.history_availability});
    fs::write(directory.join("result.json"),json_bytes(&result).unwrap()).unwrap();
    fs::write(directory.join("native-authored.nq"),prepared.rdf.as_bytes()).unwrap();
    fs::write(directory.join("native-workspace.json"),json_bytes(&prepared.snapshot).unwrap()).unwrap();
}

#[test]
fn saved_boolean_emitter_never_infers_permission_or_direction_from_defaults() {
    use oxigraph::model::{Quad,NamedNode,Literal,GraphName};
    for (root,key,predicate) in [("documents","readOnly","http://mnemosyne.dev/doc#readOnly"),("wires","bidirectional","http://mnemosyne.ai/vocab#bidirectional")] {
        for value in [false,true] {
            let subject=NamedNode::new("urn:synthetic:entity").unwrap();
            let predicate=NamedNode::new(predicate).unwrap();
            let dt=NamedNode::new("http://www.w3.org/2001/XMLSchema#boolean").unwrap();
            let quad=Quad::new(subject.clone(),predicate.clone(),Literal::new_typed_literal(if value {"True"} else {"False"},dt.clone()),GraphName::DefaultGraph);
            let projected=[format!("{subject} {predicate} {}",Literal::new_typed_literal(value.to_string(),dt.clone()))].into_iter().collect();
            let authority=HashMap::from([(subject.as_str().to_string(),json!({key:{"root":root,"entityId":"entity","key":key,"presence":"present","value":value}}))]);
            assert!(legacy_boolean_normalization(&quad,&projected,&authority).is_some());
            for (field,changed) in [("value",json!(!value)),("value",json!(value.to_string())),("value",Value::Null),("presence",json!("absent")),("presence",json!("null"))] {
                let mut wrong=authority.clone();wrong.get_mut(subject.as_str()).unwrap()[key][field]=changed;
                assert!(legacy_boolean_normalization(&quad,&projected,&wrong).is_none());
            }
            let opposite=[format!("{subject} {predicate} {}",Literal::new_typed_literal((!value).to_string(),dt))].into_iter().collect();
            assert!(legacy_boolean_normalization(&quad,&opposite,&authority).is_none());
            assert!(legacy_boolean_normalization(&quad,&projected,&HashMap::new()).is_none());
        }
    }
}

#[test]
fn saved_scalar_disposition_keeps_null_absent_and_unequal_values_distinct() {
    use oxigraph::model::{Quad,NamedNode,Literal,GraphName};
    let subject=NamedNode::new("urn:synthetic:wire").unwrap();
    let predicate=NamedNode::new("http://mnemosyne.dev/doc#createdAt").unwrap();
    let dt=NamedNode::new("http://www.w3.org/2001/XMLSchema#dateTime").unwrap();
    let quad=Quad::new(subject.clone(),predicate,Literal::new_typed_literal("2026-06-07T00:13:16Z",dt.clone()),GraphName::DefaultGraph);
    let empty=std::collections::HashSet::new();
    for presence in ["absent","null"] {
        let authority=HashMap::from([(subject.as_str().to_string(),json!({"createdAt":{"root":"wires","entityId":"wire","key":"createdAt","presence":presence,"value":null}}))]);
        let result=legacy_saved_scalar_disposition(&quad,&empty,&authority).unwrap();
        assert_eq!(result["authoritativeProjectionField"]["presence"],presence);
        assert!(result["native"].is_null());
        let mut wrong=authority.clone();wrong.get_mut(subject.as_str()).unwrap()["createdAt"]["root"]=json!("documents");
        assert!(legacy_saved_scalar_disposition(&quad,&empty,&wrong).is_none());
    }
    let predicate=NamedNode::new("http://mnemosyne.dev/doc#updatedAt").unwrap();
    let quad=Quad::new(subject.clone(),predicate.clone(),Literal::new_typed_literal("2000",dt),GraphName::DefaultGraph);
    let authority=HashMap::from([(subject.as_str().to_string(),json!({"updatedAt":{"root":"documents","entityId":"document","key":"updatedAt","presence":"present","value":1000.0}}))]);
    let projection=[format!("{subject} {predicate} {}",Literal::new_simple_literal("1000"))].into_iter().collect();
    assert!(legacy_saved_scalar_disposition(&quad,&projection,&authority).is_some());
    let mut wrong=authority.clone();wrong.get_mut(subject.as_str()).unwrap()["updatedAt"]["value"]=json!(1e100);
    let exponent=[format!("{subject} {predicate} {}",Literal::new_simple_literal("1e100"))].into_iter().collect();
    assert!(legacy_saved_scalar_disposition(&quad,&exponent,&wrong).is_none());
    assert!(legacy_saved_scalar_disposition(&quad,&empty,&authority).is_none());
    let target=NamedNode::new("urn:synthetic:other-wire").unwrap();
    let predicate=NamedNode::new("http://mnemosyne.ai/vocab#inverseOf").unwrap();
    let quad=Quad::new(subject.clone(),predicate.clone(),target.clone(),GraphName::DefaultGraph);
    let authority=HashMap::from([
        (subject.as_str().to_string(),json!({"inverseOf":{"root":"wires","entityId":"wire","key":"inverseOf","presence":"absent","value":null}})),
        (target.as_str().to_string(),json!({"inverseOf":{"root":"wires","entityId":"other-wire","key":"inverseOf","presence":"absent","value":null}})),
    ]);
    assert_eq!(legacy_saved_scalar_disposition(&quad,&empty,&authority).unwrap()["source"],json!({"iri":target.as_str()}));
    let mut wrong=authority.clone();wrong.remove(target.as_str());
    assert!(legacy_saved_scalar_disposition(&quad,&empty,&wrong).is_none());
    let projected=[format!("{subject} {predicate} {target}")].into_iter().collect();
    assert!(legacy_saved_scalar_disposition(&quad,&projected,&authority).is_none());
    let mut wrong=authority.clone();wrong.get_mut(subject.as_str()).unwrap()["inverseOf"]["presence"]=json!("present");
    assert!(legacy_saved_scalar_disposition(&quad,&empty,&wrong).is_none());
}

#[test]
fn remediation_a_saved_titles_require_exact_saved_and_native_authority() {
    use oxigraph::model::{Quad,NamedNode,Literal,GraphName};
    let subject=NamedNode::new(crate::rdf::document_subject("document")).unwrap();
    let predicate=NamedNode::new(format!("{}title",crate::runtime_config::DCTERMS_NS)).unwrap();
    for (saved,source,expected,reason) in [("","","Untitled","saved-empty-document-title-native-fallback-v1"),("Current","Old","Current","saved-document-title-projection-authority-v1")] {
        let quad=Quad::new(subject.clone(),predicate.clone(),Literal::new_simple_literal(source),GraphName::DefaultGraph);
        let authority=HashMap::from([(subject.as_str().to_string(),json!({"title":{"root":"documents","entityId":"document","key":"title","presence":"present","value":saved}}))]);
        let projected=[format!("{subject} {predicate} {}",Literal::new_simple_literal(expected))].into_iter().collect();
        let entry=legacy_saved_title_disposition(&quad,&projected,&authority).unwrap();
        assert_eq!(entry["reason"],reason);assert_eq!(entry["authoritativeProjectionField"]["value"],saved);
        assert_eq!(entry["source"]["lexical"],source);assert_eq!(entry["native"]["lexical"],expected);
        for (field,value) in [("root",json!("wires")),("presence",json!("absent")),("presence",json!("null")),("value",Value::Null),("value",json!(true)),("value",json!("different"))] {
            let mut wrong=authority.clone();wrong.get_mut(subject.as_str()).unwrap()["title"][field]=value;
            assert!(legacy_saved_title_disposition(&quad,&projected,&wrong).is_none());
        }
        assert!(legacy_saved_title_disposition(&quad,&std::collections::HashSet::new(),&authority).is_none());
        let mut wrong=quad.clone();wrong.predicate=NamedNode::new("urn:permission:title").unwrap();
        assert!(legacy_saved_title_disposition(&wrong,&projected,&authority).is_none());
        let mut duplicate=projected.clone();duplicate.insert(format!("{subject} {predicate} {}",Literal::new_simple_literal("another")));
        assert!(legacy_saved_title_disposition(&quad,&duplicate,&authority).is_none());
        let mut equal=quad.clone();equal.object=Literal::new_simple_literal(expected).into();
        assert!(legacy_saved_title_disposition(&equal,&projected,&authority).is_none());
    }
}

#[test]
fn remediation_a_cached_preview_and_access_dispositions_are_finite() {
    use oxigraph::model::{Quad,NamedNode,Literal,GraphName};
    let subject=NamedNode::new("urn:synthetic:wire").unwrap();
    let empty=std::collections::HashSet::new();
    for key in ["snapshotAt","sourceSnippet","targetSnippet"] {
        let predicate=NamedNode::new(format!("{}{key}",crate::runtime_config::WIRE_NS)).unwrap();
        let literal=if key=="snapshotAt" { Literal::new_typed_literal("2026-01-01T00:00:00Z",NamedNode::new("http://www.w3.org/2001/XMLSchema#dateTime").unwrap()) } else {Literal::new_simple_literal("cached preview")};
        let quad=Quad::new(subject.clone(),predicate.clone(),literal.clone(),GraphName::DefaultGraph);
        let authority=HashMap::from([(subject.as_str().to_string(),json!({key:{"root":"wires","entityId":"wire","key":key,"presence":"absent","value":null}}))]);
        assert!(legacy_saved_scalar_disposition(&quad,&empty,&authority).is_some());
        for (field,value) in [("presence",json!("null")),("presence",json!("present")),("root",json!("documents")),("value",json!("value"))] {
            let mut wrong=authority.clone();wrong.get_mut(subject.as_str()).unwrap()[key][field]=value;
            assert!(legacy_saved_scalar_disposition(&quad,&empty,&wrong).is_none());
        }
        let projected=[format!("{subject} {predicate} {literal}")].into_iter().collect();
        assert!(legacy_saved_scalar_disposition(&quad,&projected,&authority).is_none());
    }
    let predicate=NamedNode::new(format!("{}lastAccessedAt",crate::runtime_config::MDOC_NS)).unwrap();
    let source=Literal::new_typed_literal("2026-01-02T00:00:00Z",NamedNode::new("http://www.w3.org/2001/XMLSchema#dateTime").unwrap());
    let quad=Quad::new(subject.clone(),predicate.clone(),source,GraphName::DefaultGraph);
    let authority=HashMap::from([(subject.as_str().to_string(),json!({"lastAccessedAt":{"root":"documents","entityId":"document","key":"lastAccessedAt","presence":"present","value":"2026-01-01T00:00:00Z"}}))]);
    let projected=[format!("{subject} {predicate} {}",Literal::new_simple_literal("2026-01-01T00:00:00Z"))].into_iter().collect();
    assert!(legacy_saved_scalar_disposition(&quad,&projected,&authority).is_some());
    for root in ["wires","folders","artifacts"] {
        let mut wrong=authority.clone();wrong.get_mut(subject.as_str()).unwrap()["lastAccessedAt"]["root"]=json!(root);
        assert!(legacy_saved_scalar_disposition(&quad,&projected,&wrong).is_none());
    }
    assert!(legacy_saved_scalar_disposition(&quad,&empty,&authority).is_none());
}

fn remediation_a_source_only_wire_rdf() -> String {
    let source="urn:mnemosyne:user:owner:graph:source";
    let wire=format!("{source}:wire:legacy-wire");
    [
        format!("<{wire}> <{}> <{}Wire> <{source}> .",crate::runtime_config::RDF_TYPE,crate::runtime_config::WIRE_NS),
        format!("<{wire}> <{}sourceDocument> <{source}:doc:absent> <{source}> .",crate::runtime_config::WIRE_NS),
        format!("<{wire}> <{}sourceBlock> <{source}:doc:absent#block-part> <{source}> .",crate::runtime_config::WIRE_NS),
        format!("<{wire}> <{}targetDocument> <urn:mnemosyne:user:owner:graph:other:doc:target> <{source}> .",crate::runtime_config::WIRE_NS),
        format!("<{wire}> <{}targetGraph> \"other\" <{source}> .",crate::runtime_config::WIRE_NS),
        format!("<{wire}> <{}predicate> <urn:relation:linksTo> <{source}> .",crate::runtime_config::WIRE_NS),
        format!("<{wire}> <{}bidirectional> \"False\"^^<http://www.w3.org/2001/XMLSchema#boolean> <{source}> .",crate::runtime_config::WIRE_NS),
    ].join("\n")
}

#[test]
fn remediation_a_numeric_string_timestamp_and_explicit_null_are_source_qualified() {
    use oxigraph::model::{Quad,NamedNode,Literal,GraphName};
    let subject=NamedNode::new(crate::rdf::document_subject("doc")).unwrap();
    let predicate=NamedNode::new(format!("{}createdAt",crate::runtime_config::MDOC_NS)).unwrap();
    let datatype=NamedNode::new("http://www.w3.org/2001/XMLSchema#dateTime").unwrap();
    let quad=Quad::new(subject.clone(),predicate.clone(),Literal::new_typed_literal("1776484497470.0",datatype.clone()),GraphName::DefaultGraph);
    let authority=HashMap::from([(subject.as_str().to_string(),json!({"createdAt":{"root":"documents","entityId":"doc","key":"createdAt","presence":"present","value":"1776484497470.0"}}))]);
    let projected=[format!("{subject} {predicate} {}",Literal::new_simple_literal("1776484497470"))].into_iter().collect();
    assert!(legacy_epoch_decimal_normalization(&quad,&projected,&authority).is_some());
    for value in [json!("1776484497471.0"),json!("1e100"),Value::Null,json!(false)] {
        let mut wrong=authority.clone();wrong.get_mut(subject.as_str()).unwrap()["createdAt"]["value"]=value;
        assert!(legacy_epoch_decimal_normalization(&quad,&projected,&wrong).is_none());
    }
    let predicate=NamedNode::new(format!("{}updatedAt",crate::runtime_config::MDOC_NS)).unwrap();
    let quad=Quad::new(subject.clone(),predicate,Literal::new_typed_literal("1776484499256.0",datatype),GraphName::DefaultGraph);
    let authority=HashMap::from([(subject.as_str().to_string(),json!({"updatedAt":{"root":"documents","entityId":"doc","key":"updatedAt","presence":"null","value":null}}))]);
    let empty=std::collections::HashSet::new();
    assert!(legacy_saved_scalar_disposition(&quad,&empty,&authority).is_some());
    for (field,value) in [("presence",json!("absent")),("root",json!("folders")),("value",json!(0))] {
        let mut wrong=authority.clone();wrong.get_mut(subject.as_str()).unwrap()["updatedAt"][field]=value;
        assert!(legacy_saved_scalar_disposition(&quad,&empty,&wrong).is_none());
    }
}

#[test]
fn remediation_a_source_only_wires_require_complete_anatomy_scope_and_no_incoming_refs() {
    let rdf=remediation_a_source_only_wire_rdf();
    let snapshot=json!({"documents":[],"wires":[]});
    let empty=std::collections::HashSet::new();
    let check=|rdf:&str,snapshot:&Value,native:&std::collections::HashSet<String>|legacy_source_only_wire_subjects(rdf,snapshot,native,"owner","source","target");
    let expected="urn:mnemosyne:user:owner:graph:source:wire:legacy-wire";
    assert_eq!(check(&rdf,&snapshot,&empty).unwrap(),[expected.to_string()].into_iter().collect());
    for bad in [
        rdf.lines().filter(|line|!line.contains("rdf-syntax-ns#type")).collect::<Vec<_>>().join("\n"),
        format!("{rdf}\n{}",rdf.lines().next().unwrap()),
        rdf.replace("vocab#Wire>","vocab#Other>"),
        rdf.replace("user:owner:graph:other:doc:target","user:foreign:graph:other:doc:target"),
        rdf.replace("vocab#targetGraph","vocab#permission"),
        rdf.replace("#block-part","#block-../part"),
        rdf.replace("source:doc:absent#block-part","source:doc:another#block-part"),
        rdf.replace("\"other\"","\"wrong-graph\""),
        rdf.replace("\"False\"","\"perhaps\""),
        format!("{rdf}\n<urn:authored> <urn:linksTo> <{expected}> <urn:mnemosyne:user:owner:graph:source> ."),
        format!("{rdf}\n<urn:authored> <{expected}> <urn:object> <urn:mnemosyne:user:owner:graph:source> ."),
        format!("{rdf}\n<{expected}> <urn:auth:grant> \"admin\" <urn:mnemosyne:user:owner:graph:source> ."),
    ] { assert!(check(&bad,&snapshot,&empty).is_err()); }
    let saved=json!({"documents":[],"wires":[{"id":"legacy-wire"}]});
    assert!(check(&rdf,&saved,&empty).unwrap().is_empty());
    let native=["<urn:mnemosyne:local:graph:target:wire:legacy-wire>".to_string()].into_iter().collect();
    assert!(check(&rdf,&snapshot,&native).is_err());
    let foreign_graph=rdf.replace(" <urn:mnemosyne:user:owner:graph:source> ."," <urn:foreign:graph> .");
    assert!(check(&foreign_graph,&snapshot,&empty).is_err());
    // A document subject/tree is not made eligible by mentioning an absent ID.
    let document="<urn:mnemosyne:user:owner:graph:source:doc:orphan> <http://mnemosyne.dev/doc#content> \"authored\" <urn:mnemosyne:user:owner:graph:source> .";
    assert!(check(document,&snapshot,&empty).unwrap().is_empty());
}

#[test]
fn legacy_inverse_wires_remain_scoped_source_evidence() {
    let rdf=remediation_a_source_only_wire_rdf();
    let peer=rdf.replace("wire:legacy-wire", "wire:inverse-wire");
    let edge="<urn:mnemosyne:user:owner:graph:source:wire:legacy-wire> <http://mnemosyne.ai/vocab#inverseOf> <urn:mnemosyne:user:owner:graph:source:wire:inverse-wire> <urn:mnemosyne:user:owner:graph:source> .";
    let combined=format!("{rdf}\n{peer}\n{edge}");
    let snapshot=json!({"documents":[],"wires":[]});
    let native=std::collections::HashSet::new();
    let check=|text:&str|legacy_source_only_wire_subjects(text,&snapshot,&native,"owner","source","target");
    assert_eq!(check(&combined).unwrap().len(),2);
    let saved=json!({"documents":[],"wires":[{"id":"legacy-wire"}]});
    assert_eq!(legacy_source_only_wire_subjects(&combined,&saved,&native,"owner","source","target").unwrap().len(),1);
    let bad=edge.replace("user:owner:graph:source:wire:legacy-wire", "user:foreign:graph:source:wire:legacy-wire");
    assert!(legacy_source_only_wire_subjects(&format!("{rdf}\n{peer}\n{bad}"),&saved,&native,"owner","source","target").is_err());
    assert!(check(&format!("{rdf}\n{}",edge.replace("user:owner:graph:source:wire:inverse-wire", "user:foreign:graph:source:wire:inverse-wire"))).is_err());
    assert!(check(&format!("{combined}\n<urn:authored> <urn:linksTo> <urn:mnemosyne:user:owner:graph:source:wire:inverse-wire> <urn:mnemosyne:user:owner:graph:source> .")).is_err());
}

#[test]
fn remediation_a_source_only_graph_mapping_is_context_qualified_and_queryable_after_reopen() {
    use oxigraph::model::NamedNode;
    let rdf=remediation_a_source_only_wire_rdf();
    let graph=legacy_source_evidence_graph("owner","source","target",&"a".repeat(64),"urn:mnemosyne:user:owner:graph:source");
    for other in [legacy_source_evidence_graph("owner","source","target",&"b".repeat(64),"urn:mnemosyne:user:owner:graph:source"),
        legacy_source_evidence_graph("other","source","target",&"a".repeat(64),"urn:mnemosyne:user:owner:graph:source"),
        legacy_source_evidence_graph("owner","source","target",&"a".repeat(64),"urn:mnemosyne:user:owner:graph:other")] {assert_ne!(graph,other);}
    let temp=std::env::temp_dir().join(format!("source-only-rdf-{}",uuid::Uuid::new_v4()));
    let mut expected=std::collections::BTreeSet::new();
    {
        let store=oxigraph::store::Store::open(&temp).unwrap();
        for quad in RdfParser::from_format(RdfFormat::NQuads).for_slice(rdf.as_bytes()) {
            let mut quad=quad.unwrap();quad.graph_name=NamedNode::new(&graph).unwrap().into();
            expected.insert(quad.to_string());store.insert(&quad).unwrap();
        }
        store.flush().unwrap();
        assert_eq!(store.len().unwrap(),expected.len());
    }
    let store=oxigraph::store::Store::open(&temp).unwrap();
    let actual:std::collections::BTreeSet<_>=store.iter().map(|q|q.unwrap().to_string()).collect();
    assert_eq!(actual,expected);
    assert!(actual.iter().all(|q|!q.contains("urn:mnemosyne:local:document:")&&!q.contains(":projection:workspace")));
    let query=format!("SELECT ?s ?p ?o WHERE {{ GRAPH <{graph}> {{ ?s ?p ?o }} }}");
    let result=store.query(query.as_str()).unwrap();
    let oxigraph::sparql::QueryResults::Solutions(rows)=result else{panic!("expected rows");};
    assert_eq!(rows.map(Result::unwrap).count(),expected.len());
}

#[test]
fn legacy_workflow_types_are_finite_authored_assertions_and_wire_emitter_requires_wire() {
    use oxigraph::model::{Quad,NamedNode,Literal,GraphName};
    let subject=NamedNode::new("urn:synthetic:wire").unwrap();
    let rdf_type=NamedNode::new(crate::runtime_config::RDF_TYPE).unwrap();
    for kind in ["AgentNode","AgentRun","Run","Variant","Archetype","Phase","Workflow"] {
        let quad=Quad::new(subject.clone(),rdf_type.clone(),NamedNode::new(format!("http://mnemosyne.dev/workflow#{kind}")).unwrap(),GraphName::DefaultGraph);
        assert!(legacy_authored_workflow_type(&quad));
    }
    for kind in ["http://mnemosyne.dev/workflow#Permission","http://mnemosyne.dev/doc#Document","urn:unknown:Type"] {
        let quad=Quad::new(subject.clone(),rdf_type.clone(),NamedNode::new(kind).unwrap(),GraphName::DefaultGraph);
        assert!(!legacy_authored_workflow_type(&quad));
    }
    let predicate=NamedNode::new("http://mnemosyne.dev/doc#createdAt").unwrap();
    let dt=NamedNode::new("http://www.w3.org/2001/XMLSchema#dateTime").unwrap();
    let quad=Quad::new(subject.clone(),predicate.clone(),Literal::new_typed_literal("2026-06-07T00:13:16.228378Z",dt.clone()),GraphName::DefaultGraph);
    let mut projection:std::collections::HashSet<String>=[format!("{subject} {predicate} {}",Literal::new_typed_literal("1780791196228.378",dt))].into_iter().collect();
    assert!(legacy_projection_normalization(&quad,&projection).is_none());
    projection.insert(format!("{subject} {rdf_type} <{}Wire>",crate::runtime_config::WIRE_NS));
    assert_eq!(legacy_projection_normalization(&quad,&projection).unwrap()["rule"],"cloud1-python-epoch-to-rfc3339");
    let projected=[format!("{subject} {predicate} {}",Literal::new_typed_literal("2026-06-07T00:13:16.228378+00:00",NamedNode::new("http://www.w3.org/2001/XMLSchema#dateTime").unwrap())),
        format!("{subject} {rdf_type} <{}Wire>",crate::runtime_config::WIRE_NS)].into_iter().collect();
    assert_eq!(legacy_projection_normalization(&quad,&projected).unwrap()["rule"],"rfc3339-equal-utc-instant");
}

#[test]
fn legacy_absent_access_is_historical_only_and_requires_exact_absence() {
    use oxigraph::model::{Quad,NamedNode,Literal,GraphName};
    let subject=NamedNode::new("urn:synthetic:document").unwrap();
    let predicate=NamedNode::new("http://mnemosyne.dev/doc#lastAccessedAt").unwrap();
    let datatype=NamedNode::new("http://www.w3.org/2001/XMLSchema#dateTime").unwrap();
    let authority=HashMap::from([(subject.as_str().to_string(),json!({"root":"documents","entityId":"synthetic","key":"lastAccessedAt","presence":"absent"}))]);
    let empty=std::collections::HashSet::new();
    for value in ["1780791196228.378","2026-06-07T00:13:16.228378Z"] {
        let quad=Quad::new(subject.clone(),predicate.clone(),Literal::new_typed_literal(value,datatype.clone()),GraphName::DefaultGraph);
        let result=legacy_absent_access_disposition(&quad,&empty,&authority).unwrap();
        assert!(result["native"].is_null());
        assert_eq!(result["source"]["lexical"],value);
        assert!(legacy_absent_access_disposition(&quad,&empty,&HashMap::new()).is_none());
        let projected=[format!("{subject} {predicate} {}",Literal::new_simple_literal("other"))].into_iter().collect();
        assert!(legacy_absent_access_disposition(&quad,&projected,&authority).is_none());
        for (key,value) in [("presence",json!("present")),("key",json!("owner")),("root",json!("foreign")),("entityId",json!(""))] {
            let mut changed=authority.clone();changed.get_mut(subject.as_str()).unwrap()[key]=value;
            assert!(legacy_absent_access_disposition(&quad,&empty,&changed).is_none());
        }
    }
    for (p,v,dt) in [("lastAccessedAt","NaN",datatype.as_str()),("lastAccessedAt","-1",datatype.as_str()),
        ("owner","123",datatype.as_str()),("createdAt","123",datatype.as_str()),
        ("lastAccessedAt","123","http://www.w3.org/2001/XMLSchema#string")] {
        let quad=Quad::new(subject.clone(),NamedNode::new(format!("http://mnemosyne.dev/doc#{p}")).unwrap(),Literal::new_typed_literal(v,NamedNode::new(dt).unwrap()),GraphName::DefaultGraph);
        assert!(legacy_absent_access_disposition(&quad,&empty,&authority).is_none());
    }
}

#[test]
fn unequal_derived_order_requires_actual_field_and_native_projection_not_equivalence() {
    use oxigraph::model::{Quad,NamedNode,Literal,GraphName};
    let subject=NamedNode::new("urn:mnemosyne:local:document:synthetic").unwrap();
    let predicate=NamedNode::new("http://mnemosyne.dev/doc#order").unwrap();
    let datatype=NamedNode::new("http://www.w3.org/2001/XMLSchema#float").unwrap();
    let quad=Quad::new(subject.clone(),predicate.clone(),Literal::new_typed_literal("1000",datatype.clone()),GraphName::DefaultGraph);
    let projected=[format!("{subject} {predicate} {}",Literal::new_typed_literal("2",datatype.clone()))].into_iter().collect();
    let authority=HashMap::from([(subject.as_str().to_string(),json!({"root":"documents","entityId":"synthetic","key":"order","value":2.0}))]);
    assert!(legacy_projection_normalization(&quad,&projected).is_none());
    let disposition=legacy_order_disposition(&quad,&projected,&authority).unwrap();
    assert_eq!(disposition["disposition"],"retained-not-rematerialized");
    assert_eq!(disposition["source"]["lexical"],"1000");
    assert_eq!(disposition["native"]["lexical"],"2");
    assert!(legacy_order_disposition(&quad,&projected,&HashMap::new()).is_none());
    assert!(legacy_order_disposition(&quad,&std::collections::HashSet::new(),&authority).is_none());
    for (key,value) in [("value",Value::Null),("value",json!(3)),("key",json!("title")),("root",json!("foreign"))] {
        let mut changed=authority.clone();changed.get_mut(subject.as_str()).unwrap()[key]=value;
        assert!(legacy_order_disposition(&quad,&projected,&changed).is_none());
    }
    for (s,p,v) in [("urn:foreign",predicate.as_str(),"1000"),
        (subject.as_str(),"http://mnemosyne.dev/doc#title","1000"),
        (subject.as_str(),predicate.as_str(),"NaN"),(subject.as_str(),predicate.as_str(),"2")] {
        let wrong=Quad::new(NamedNode::new(s).unwrap(),NamedNode::new(p).unwrap(),Literal::new_typed_literal(v,datatype.clone()),GraphName::DefaultGraph);
        assert!(legacy_order_disposition(&wrong,&projected,&authority).is_none());
    }
}

#[test]
fn legacy_projection_rules_match_python_emitter_and_declared_float_value_space() {
    use oxigraph::model::{Quad,NamedNode,Literal,GraphName};
    let subject=NamedNode::new("urn:synthetic:document").unwrap();
    let timestamp=NamedNode::new("http://mnemosyne.dev/doc#createdAt").unwrap();
    // Produced by an actual finite CPython datetime.fromtimestamp(ms/1000,
    // timezone.utc) run, including sub-microsecond and second-carry edges.
    for (millis,iso) in [
        ("1774991543203.46","2026-03-31T21:12:23.203460+00:00"),
        ("1780791196228.378","2026-06-07T00:13:16.228378+00:00"),
        ("1780791196228.0002","2026-06-07T00:13:16.228000+00:00"),
        ("1780791196228.9998","2026-06-07T00:13:16.229000+00:00"),
        ("1780791196999.9998","2026-06-07T00:13:17+00:00"),
        ("1780791196000.0002","2026-06-07T00:13:16+00:00"),
    ] {
        let quad=Quad::new(subject.clone(),timestamp.clone(),Literal::new_typed_literal(iso,
            NamedNode::new("http://www.w3.org/2001/XMLSchema#dateTime").unwrap()),GraphName::DefaultGraph);
        let projection=[format!("{subject} {timestamp} {}",Literal::new_simple_literal(millis))].into_iter().collect();
        let result=legacy_projection_normalization(&quad,&projection).unwrap();
        assert_eq!(result["rule"],"cloud1-python-epoch-to-rfc3339");
        assert_eq!(result["native"]["lexical"],millis);
        let changed=chrono::DateTime::parse_from_rfc3339(iso).unwrap()+chrono::Duration::microseconds(1);
        let wrong=Quad::new(subject.clone(),timestamp.clone(),Literal::new_typed_literal(changed.to_rfc3339(),
            NamedNode::new("http://www.w3.org/2001/XMLSchema#dateTime").unwrap()),GraphName::DefaultGraph);
        assert!(legacy_projection_normalization(&wrong,&projection).is_none());
    }
    let order=NamedNode::new("http://mnemosyne.dev/doc#order").unwrap();
    let datatype=NamedNode::new("http://www.w3.org/2001/XMLSchema#float").unwrap();
    let native=Literal::new_typed_literal("1774991543203.46",datatype.clone());
    let projection=[format!("{subject} {order} {native}")].into_iter().collect();
    let source=Quad::new(subject.clone(),order.clone(),Literal::new_typed_literal("1774991600000",datatype.clone()),GraphName::DefaultGraph);
    assert_eq!(legacy_projection_normalization(&source,&projection).unwrap()["rule"],"xsd-float-value-space");
    for value in ["1774992600000","NaN","INF","-INF"] {
        let wrong=Quad::new(subject.clone(),order.clone(),Literal::new_typed_literal(value,datatype.clone()),GraphName::DefaultGraph);
        assert!(legacy_projection_normalization(&wrong,&projection).is_none());
    }
}

#[test]
fn legacy_timestamp_datatype_repair_requires_exact_native_lexical_projection() {
    use oxigraph::model::{Quad, NamedNode, Literal, GraphName};
    let subject = NamedNode::new("urn:test:document").unwrap();
    let predicate = NamedNode::new("http://mnemosyne.dev/doc#createdAt").unwrap();
    let timestamp = "1774991543203.46";
    let quad = Quad::new(subject.clone(),predicate.clone(),Literal::new_typed_literal(timestamp,
        NamedNode::new("http://www.w3.org/2001/XMLSchema#dateTime").unwrap()),GraphName::DefaultGraph);
    let projected = [format!("{subject} {predicate} {}",Literal::new_simple_literal(timestamp))].into_iter().collect();
    assert!(legacy_projection_normalization(&quad,&projected).is_some());
    let rfc = "2026-06-07T00:13:16.228378Z";
    let rfc_quad = Quad::new(subject.clone(),predicate.clone(),Literal::new_typed_literal(rfc,
        NamedNode::new("http://www.w3.org/2001/XMLSchema#dateTime").unwrap()),GraphName::DefaultGraph);
    let rfc_projected = [format!("{subject} {predicate} {}",Literal::new_simple_literal(rfc))].into_iter().collect();
    assert!(legacy_projection_normalization(&rfc_quad,&rfc_projected).is_some());
    assert!(legacy_projection_normalization(&rfc_quad,&projected).is_none());
    for (s,p,v,datatype) in [("urn:test:foreign",predicate.as_str(),timestamp,"dateTime"),
        (subject.as_str(),"http://mnemosyne.dev/doc#title",timestamp,"dateTime"),
        (subject.as_str(),predicate.as_str(),"1774991543203.460","dateTime"),
        (subject.as_str(),predicate.as_str(),"1774991543203.47","dateTime"),
        (subject.as_str(),predicate.as_str(),timestamp,"decimal")] {
        let changed = Quad::new(NamedNode::new(s).unwrap(),NamedNode::new(p).unwrap(),
            Literal::new_typed_literal(v,NamedNode::new(format!("http://www.w3.org/2001/XMLSchema#{datatype}")).unwrap()),GraphName::DefaultGraph);
        assert!(legacy_projection_normalization(&changed,&projected).is_none());
    }
}

#[test]
fn legacy_history_list_wrapper_projection_is_checked_against_native_tree() {
    let xml = "<listItem data-block-id=\"list-1\" listType=\"bullet\" indent=\"0\"><paragraph data-block-id=\"p-1\" indent=\"0\" collapsed=\"false\">A <strong>marked</strong> line</paragraph></listItem>";
    let source = vec![json!({"id":"p-1","type":"paragraph","text":"A <strong>marked</strong> line",
        "parent_id":null,"index":0,"order":0,"collapsed":false,"properties":{"indent":0.0,"collapsed":false}})];
    let parsed = crate::crdt_engine::content_parse::parse_write_content_for_operation(xml,Some("xml"),"test").unwrap();
    assert!(parsed.warnings.is_empty());
    let projection = crate::crdt_engine::projection::materialize_tiptap_json(&parsed.tiptap_json,"test");
    assert_eq!(projection.blocks_json[0]["id"],"list-1");
    validate_legacy_history_blocks(xml,&source,&parsed.tiptap_json).unwrap();
    for (key, value) in [("id",json!("foreign")),("order",json!(1)),("index",json!(1)),
        ("parent_id",json!("foreign")),("type",json!("heading")),("text",json!("A marked line changed")),
        ("properties",json!({"indent":2,"collapsed":false}))] {
        let mut changed = source.clone();
        changed[0][key] = value;
        assert!(validate_legacy_history_blocks(xml,&changed,&parsed.tiptap_json).is_err(),"{key}");
    }
    let mut changed = parsed.tiptap_json.clone();
    changed["content"][0]["content"][0]["content"][1]["marks"] = json!([]);
    assert!(validate_legacy_history_blocks(xml,&source,&changed).is_err());
    let mut duplicate = source.clone(); duplicate.push(source[0].clone());
    assert!(validate_legacy_history_blocks(xml,&duplicate,&parsed.tiptap_json).is_err());
}

#[test]
fn legacy_history_inline_text_requires_exact_content_and_mark_spans() {
    let text = "plain <strong>bold &amp; true</strong> <code>x</code>";
    let parsed = crate::crdt_engine::content_parse::parse_write_content_for_operation(
        &format!("<paragraph data-block-id=\"b\">{}</paragraph>",encode_legacy_history_text(text,false).unwrap()),Some("xml"),"fixture").unwrap();
    let projection = crate::crdt_engine::projection::materialize_tiptap_json(&parsed.tiptap_json,"fixture");
    let mut blocks: Vec<crate::document_types::BlockSnapshot> = serde_json::from_value(projection.blocks_json).unwrap();
    assert!(legacy_history_marked_text_matches(text,&blocks[0],None).unwrap());
    let original = blocks[0].clone();
    blocks[0].content.push('!');
    assert!(!legacy_history_marked_text_matches(text,&blocks[0],None).unwrap());
    blocks[0] = original.clone();
    blocks[0].marks.clear();
    assert!(!legacy_history_marked_text_matches(text,&blocks[0],None).unwrap());
    for bad in ["<unknown>text</unknown>","<strong secret=\"x\">text</strong>","<strong>unclosed"] {
        assert!(legacy_history_marked_text_matches(bad,&original,None).is_err());
    }
}

fn actual_empty_rdf_archive() -> Vec<u8> {
    let bytes = fs::read(std::env::var("GARDEN_PRESERVATION_EMPTY_RDF_ARCHIVE")
        .expect("actual empty-RDF saved archive required")).unwrap();
    assert_eq!(archive_sha256(&bytes),
        "5e7457767e1c6696a4f4ba669e7d19935a4ea331a5379fca00935d231b8486f8",
        "empty-RDF archive pin changed");
    bytes
}

#[test]
fn actual_empty_rdf_keeps_workspace_history_and_catalogue_without_synthetic_quad() {
    let bytes = actual_empty_rdf_archive();
    let members = unpack(&bytes).unwrap();
    let prepared = prepare(&bytes, &manifest_operation(&bytes)).unwrap();
    assert!(prepared.members == members, "source custody changed");
    assert!(members["rdf/dataset.nq"].is_empty());
    assert!(prepared.parsed.documents.is_empty());
    assert!(prepared.parsed.workspace_bytes.is_some());
    assert_eq!(prepared.graph_history.len(), 1);
    assert!(!prepared.metadata.is_empty(), "real source catalogue metadata missing");
    assert_eq!(prepared.manifest["namedGraphs"], json!([]));
    assert_eq!(prepared.graph_mapping, json!({}));
    assert!(prepared.rdf.is_empty());
    assert!(prepared.parsed.rdf_n_quads.is_empty());
    assert_eq!(prepared.regenerated, 0);
    assert_eq!(prepared.manifest["counts"]["rdfQuads"], json!(0));
    assert_eq!(prepared.manifest["counts"]["namedGraphs"], json!(0));
}

#[test]
fn empty_rdf_exception_refuses_nonempty_data_inventory_or_mismatched_counts() {
    let bytes = actual_empty_rdf_archive();
    for mutation in ["quads-count", "graphs-count", "unlisted-quad", "malformed",
        "empty-listed-main", "foreign-graph"] {
        let prepared = prepare(&bytes, &manifest_operation(&bytes)).unwrap();
        let graph = prepared.parsed.manifest.source_graph_id.clone();
        let source = format!("urn:mnemosyne:user:{}:graph:{graph}",
            prepared.parsed.manifest.source_user_id);
        let mut manifest = prepared.manifest;
        let mut parsed = prepared.parsed;
        let mut rdf = String::new();
        let expected = match mutation {
            "quads-count" => {
                manifest["counts"]["rdfQuads"] = json!(1);
                "empty RDF inventory count mismatch"
            }
            "graphs-count" => {
                manifest["counts"]["namedGraphs"] = json!(1);
                "empty RDF inventory count mismatch"
            }
            "unlisted-quad" => {
                rdf = format!("<urn:test:s> <urn:test:p> <urn:test:o> <{source}> .\n");
                "unclassified named RDF graph"
            }
            "malformed" => { rdf = "not RDF".into(); "" }
            "empty-listed-main" => {
                manifest["namedGraphs"] = json!([{"iri":source,
                    "disposition":"core-v3-authority-partition"}]);
                manifest["counts"]["namedGraphs"] = json!(1);
                "empty or missing named graph testimony"
            }
            _ => {
                manifest["namedGraphs"] = json!([{"iri":"urn:foreign:graph",
                    "disposition":"preserve-separate-authored-graph"}]);
                "foreign/derived/unknown source RDF graph"
            }
        };
        let error = prepare_rdf(&rdf, &manifest, &mut parsed, &graph).err().expect(mutation);
        assert!(error.contains(expected), "wrong empty-RDF refusal for {mutation}");
    }
}

#[test]
fn empty_rdf_still_requires_exact_source_custody_and_catalogue_identity() {
    let bytes = actual_empty_rdf_archive();
    let prepared = prepare(&bytes, &manifest_operation(&bytes)).unwrap();
    let graph = &prepared.parsed.manifest.source_graph_id;
    let user = &prepared.parsed.manifest.source_user_id;
    for field in ["userId", "graphId", "catalogueSource"] {
        let mut members = prepared.members.clone();
        let mut custody = parse_json(&members["source-custody/index.json"]).unwrap();
        if field == "catalogueSource" {
            custody[field]["key"] = json!("unretained/foreign-catalogue");
        } else {
            custody[field] = json!("different-source-identity");
        }
        members.insert("source-custody/index.json".into(), json_bytes(&custody).unwrap());
        let error = prepare_custody(&members, &prepared.manifest, graph, user).err().expect(field);
        assert!(error.contains(if field == "catalogueSource" {
            "source graph catalogue byte custody missing"
        } else { "source custody identity/authority mismatch" }));
    }
}

#[test]
fn actual_saved_state_with_orphan_history_fully_prepares_read_only() {
    let bytes = actual_orphan_archive();
    let original_members = unpack(&bytes).unwrap();
    let prepared = prepare(&bytes, &manifest_operation(&bytes)).unwrap();
    assert!(prepared.members == original_members, "source custody changed");
    assert_eq!(prepared.parsed.documents.len(), 10);
    assert_eq!(prepared.originals.len(), 0);
    assert_eq!(prepared.history.len(), 0);
    assert_eq!(prepared.graph_history.len(), 1);
    for (id, body) in &prepared.parsed.documents {
        assert!(
            body == &original_members[&format!("crdt/documents/{id}.yjs")],
            "current document bytes changed"
        );
    }
    let point = &prepared.graph_history[0];
    assert_eq!(point.manifest.schema_version, 1);
    assert_eq!(point.documents.len(), 11);
    assert!(!point.storage_only_document_ids.is_empty());
    assert!(point.workspace_only_document_ids.is_empty());
    let fallback_ids: BTreeSet<_> = point.id_title_fallbacks.iter().cloned().collect();
    assert!(fallback_ids == point.storage_only_document_ids.iter().cloned().collect());
    let source_path = format!("history/graph/{}.json", point.manifest.restore_point_id);
    let source = parse_json(&original_members[&source_path]).unwrap();
    assert_eq!(point.manifest.content_hash_sha256, archive_sha256(&original_members[&source_path]));
    let versions = parse_json(&original_members["history/graph/version-members.json"]).unwrap();
    for (id, body) in &point.documents {
        let reference = &source["documents"].as_array().unwrap().iter()
            .find(|row| row["doc_id"] == *id).unwrap()["ref"];
        let mapping = versions.as_array().unwrap().iter()
            .find(|row| row["key"] == reference["key"] && row["version_id"] == reference["version_id"])
            .unwrap();
        assert!(body.as_ref() == original_members[mapping["member"].as_str().unwrap()].as_slice(), "historical bytes changed");
        if fallback_ids.contains(id) {
            let payload = point.payloads.iter().find(|p| p.document_id == *id).unwrap();
            assert!(payload.title == *id, "orphan display title was invented");
        }
    }
    let disposition = graph_history_disposition(point);
    assert_eq!(disposition["restoreSupported"], false);
    assert_eq!(disposition["nativeSchemaVersion"], 1);
    assert!(disposition["displayTitleFallbacks"].as_array().unwrap().iter()
        .all(|row| row["sourceTitle"].is_null() && row["displayTitle"] == row["documentId"]));
}

#[test]
fn orphan_history_still_refuses_duplicate_foreign_and_unresolved_references() {
    let bytes = actual_orphan_archive();
    let prepared = prepare(&bytes, &manifest_operation(&bytes)).unwrap();
    let point = &prepared.graph_history[0];
    let source_path = format!("history/graph/{}.json", point.manifest.restore_point_id);
    for mutation in ["duplicate", "foreign-key", "foreign-owner", "unresolved", "size"] {
        let mut members = prepared.members.clone();
        let mut source = parse_json(&members[&source_path]).unwrap();
        let expected = match mutation {
            "duplicate" => {
                let duplicate = source["documents"][0].clone();
                source["documents"].as_array_mut().unwrap().push(duplicate);
                "graph history document inventory mismatch"
            }
            "foreign-key" => {
                source["documents"][0]["ref"]["key"] = json!("users/other/graphs/other/documents/other.yjs");
                "historical document key mismatch"
            }
            "foreign-owner" => {
                source["user_id"] = json!("other-owner");
                "graph history format/owner mismatch"
            }
            "unresolved" => {
                source["documents"][0]["ref"]["version_id"] = json!("uncaptured-version");
                "historical source version unresolved"
            }
            _ => {
                source["documents"][0]["ref"]["size_bytes"] = json!(0);
                "historical reference size mismatch"
            }
        };
        members.insert(source_path.clone(), json_bytes(&source).unwrap());
        // Call the semantic seam directly: these are not stale-envelope refusals.
        let error = prepare_graph_history(&members, &prepared.manifest,
            &prepared.parsed.manifest, &prepared.parsed.manifest.source_graph_id)
            .err().expect("invalid historical reference accepted");
        assert!(error.contains(expected), "wrong refusal for {mutation}");
    }
}

#[cfg(feature = "headless")]
#[test]
fn persisted_orphan_checkpoint_refuses_restore_without_any_disk_effect() {
    fn tree(root: &Path) -> BTreeMap<PathBuf, String> {
        fn visit(root: &Path, current: &Path, rows: &mut BTreeMap<PathBuf, String>) {
            for entry in fs::read_dir(current).unwrap() {
                let path = entry.unwrap().path();
                let meta = fs::symlink_metadata(&path).unwrap();
                assert!(!meta.file_type().is_symlink());
                if meta.is_dir() {
                    rows.insert(path.strip_prefix(root).unwrap().into(), "directory".into());
                    visit(root, &path, rows);
                } else {
                    assert!(meta.is_file());
                    rows.insert(path.strip_prefix(root).unwrap().into(), archive_sha256(&fs::read(path).unwrap()));
                }
            }
        }
        let mut rows = BTreeMap::new();
        visit(root, root, &mut rows);
        rows
    }
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let previous = std::env::var_os("GARDEN_PROFILE_DIR");
    let profile = std::env::temp_dir().join(format!("preservation-orphan-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&profile).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&profile, fs::Permissions::from_mode(0o700)).unwrap();
    }
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let result = std::panic::catch_unwind(|| {
        let bytes = actual_orphan_archive();
        let operation = manifest_operation(&bytes);
        let prepared = prepare(&bytes, &operation).unwrap();
        let graph = operation.graph_id.as_str();
        let graph_dir = profile.join("graphs").join(graph);
        fs::create_dir_all(&graph_dir).unwrap();
        let record = json!({"graphId":graph,"title":"Isolated historical refusal",
            "origin":"local","providerId":"local","localPath":graph_dir.to_string_lossy(),
            "createdAt":"1789150000000","updatedAt":"1789150000000","capabilities":[]});
        // A deliberately small valid local fixture; no RDF database or background worker.
        let _: crate::graph_record_store::GraphRecord = serde_json::from_value(record.clone()).unwrap();
        fs::write(graph_dir.join("graph.json"), json_bytes(&record).unwrap()).unwrap();
        let app = crate::tauri_runtime::build_mock_app_for_tests(false);
        assert!(crate::graph_paths::existing_graph_dir(&app, graph).unwrap() == graph_dir);
        persist_graph_history(&graph_dir, graph, &prepared.graph_history).unwrap();
        let point = &prepared.graph_history[0];
        let native = crate::time_travel_store::read_manifest(&graph_dir, &point.manifest.restore_point_id).unwrap();
        assert_eq!(native.schema_version, 1);
        assert_eq!(native.documents.len(), 11);
        for (id, body) in &point.documents {
            assert!(crate::time_travel_store::read_document_bytes(&graph_dir,
                &native.restore_point_id, id).unwrap().as_slice() == body.as_ref(), "persisted historical bytes changed");
        }
        let jobs = std::sync::Arc::new(crate::local_jobs::LocalJobRegistry::new(profile.join("jobs")).unwrap());
        let before = tree(&profile);
        let error = crate::time_travel_restore_service::start_restore(&app, jobs, graph,
            &native.restore_point_id, false).err().expect("schema1 restore unexpectedly admitted");
        assert!(error.message().contains("uses schema v1"));
        assert!(tree(&profile) == before, "refused restore changed files or directory inventory");
    });
    match previous {
        Some(value) => std::env::set_var("GARDEN_PROFILE_DIR", value),
        None => std::env::remove_var("GARDEN_PROFILE_DIR"),
    }
    // Keep the isolated fixture under this run's private TMPDIR for examination.
    if let Err(error) = result { std::panic::resume_unwind(error); }
}
fn pack(entries: impl IntoIterator<Item = (String, Vec<u8>)>) -> Vec<u8> {
    let encoder = GzEncoder::new(Vec::new(), Compression::fast());
    let mut builder = tar::Builder::new(encoder);
    for (path, body) in entries {
        let mut header = tar::Header::new_ustar();
        header.set_size(body.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_cksum();
        builder
            .append_data(&mut header, path, body.as_slice())
            .unwrap();
    }
    builder.into_inner().unwrap().finish().unwrap()
}
fn reseal(members: &mut Members) {
    let mut manifest = parse_json(&members["manifest.json"]).unwrap();
    let mut descriptor = parse_json(
        manifest["captureDescriptorCanonicalJson"]
            .as_str()
            .unwrap()
            .as_bytes(),
    )
    .unwrap();
    for (path, bytes) in members
        .iter()
        .filter(|(p, _)| p.as_str() != "manifest.json")
    {
        manifest["members"][path] =
            json!({"sha256":archive_sha256(bytes),"byteLength":bytes.len()});
        manifest["sourceObjects"][path]["sha256"] = json!(archive_sha256(bytes));
        manifest["sourceObjects"][path]["byte_length"] = json!(bytes.len());
        let entry = descriptor["members"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|e| e["path"] == *path)
            .unwrap();
        entry["source"] = manifest["sourceObjects"][path].clone();
    }
    let raw = json_bytes(&descriptor).unwrap();
    manifest["captureDescriptorCanonicalJson"] = json!(String::from_utf8(raw.clone()).unwrap());
    manifest["source"]["capture"]["descriptorSha256"] = json!(archive_sha256(&raw));
    members.insert("manifest.json".into(), json_bytes(&manifest).unwrap());
}

#[test]
fn actual_producer_and_capture_envelopes_preflight() {
    for v22 in [false, true] {
        let bytes = fixture(v22);
        let prepared = prepare(&bytes, &operation(&bytes)).unwrap();
        assert_eq!(prepared.parsed.documents.len(), 2);
        assert_eq!(prepared.originals.len(), 3);
        assert_eq!(prepared.history.len(), 2);
        for item in &prepared.history {
            assert!(crate::clock::parse_timestamp(&item.meta.created_at).is_some());
            assert_eq!(item.meta.created_at, item.payload.created_at);
        }
        for point in &prepared.graph_history {
            for payload in &point.payloads {
                assert_eq!(
                    crate::clock::parse_timestamp(&payload.created_at),
                    Some(point.manifest.created_at as u128)
                );
            }
        }
        assert_eq!(prepared.deletions, vec!["doc-fixture-deleted"]);
        assert_eq!(
            prepared.graph_history[0].manifest.schema_version, 1,
            "actual sparse snapshot must remain read-only"
        );
        assert_eq!(
            prepared.metadata.get("title").map(String::as_str),
            v22.then_some("Retained fixture graph")
        );
    }
}

#[test]
fn explicit_v23_logical_saved_state_admits_literal_null_version() {
    let bytes = null_fixture();
    let prepared = prepare(&bytes, &operation(&bytes)).unwrap();
    assert_eq!(prepared.parsed.documents.len(), 2);
    let custody = parse_json(&prepared.members["source-custody/index.json"]).unwrap();
    assert_eq!(
        custody["providerVersionSelection"]["selectedNullVersionCount"],
        1
    );
    assert_eq!(
        custody["nullVersionReadback"]["firstInventorySha256"],
        custody["inventorySha256"]
    );
}

#[test]
fn actual_v23_logical_saved_state_null_custody_preflights() {
    let bytes = fs::read(
        std::env::var("GARDEN_PRESERVATION_REAL_NULL_ARCHIVE")
            .expect("actual literal-null v2.3 archive required"),
    )
    .unwrap();
    let members = unpack(&bytes).unwrap();
    let manifest = parse_json(&members["manifest.json"]).unwrap();
    let operation = manifest_operation(&bytes);
    validate_envelope(&members, &manifest, &operation, &bytes).unwrap();
    let user = manifest["source"]["userId"].as_str().unwrap();
    let graph = manifest["source"]["graphId"].as_str().unwrap();
    prepare_custody(&members, &manifest, graph, user).unwrap();
}

#[test]
fn v23_admits_hash_bound_raw_only_null_custody() {
    let bytes = custody_only_null_fixture();
    let prepared = prepare(&bytes, &operation(&bytes)).unwrap();
    let manifest = parse_json(&prepared.members["manifest.json"]).unwrap();
    let custody = parse_json(&prepared.members["source-custody/index.json"]).unwrap();
    assert!(manifest["sourceObjects"]
        .as_object()
        .unwrap()
        .values()
        .all(|source| source["version_id"] != "null"));
    assert_eq!(
        custody["providerVersionSelection"]["selectedNullVersionCount"],
        2
    );
    assert_eq!(
        custody["objects"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["version_id"] == "null")
            .count(),
        2
    );
}

#[test]
fn null_semantic_descriptor_requires_exact_raw_origin() {
    let mut members = unpack(&null_fixture()).unwrap();
    let mut manifest = parse_json(&members["manifest.json"]).unwrap();
    manifest["sourceObjects"]["crdt/workspace.yjs"]["key"] = json!(
        "users/fixture-cloud1-owner-20260908/graphs/demi-cross-format-fixture-20260908/other-workspace.yjs"
    );
    manifest["source"]["capture"]["snapshotBoundaries"][0]["source"] =
        manifest["sourceObjects"]["crdt/workspace.yjs"].clone();
    let mut descriptor = parse_json(
        manifest["captureDescriptorCanonicalJson"]
            .as_str()
            .unwrap()
            .as_bytes(),
    )
    .unwrap();
    descriptor["snapshot_boundaries"] = manifest["source"]["capture"]["snapshotBoundaries"].clone();
    manifest["captureDescriptorCanonicalJson"] =
        json!(String::from_utf8(json_bytes(&descriptor).unwrap()).unwrap());
    members.insert("manifest.json".into(), json_bytes(&manifest).unwrap());
    reseal(&mut members);
    let changed = pack(members);
    let error = prepare(&changed, &operation(&changed)).err().unwrap();
    assert!(
        error.contains("semantic/custody origin mismatch"),
        "{error}"
    );
}

#[test]
fn literal_null_version_requires_exact_readback_provenance() {
    for mutation in ["mode", "count", "bucket", "readback", "writer"] {
        let bytes = null_fixture();
        let mut members = unpack(&bytes).unwrap();
        let mut custody = parse_json(&members["source-custody/index.json"]).unwrap();
        match mutation {
            "mode" => custody["providerVersionSelection"]["mode"] = json!("all"),
            "count" => custody["providerVersionSelection"]["selectedNullVersionCount"] = json!(2),
            "bucket" => {
                custody["providerVersionSelection"]["bucketVersioning"][1]["status"] =
                    json!("Suspended")
            }
            "readback" => {
                custody["nullVersionReadback"]["secondInventorySha256"] = json!("0".repeat(64))
            }
            _ => {
                custody["writerBoundary"] =
                    json!("reviewed-runtime-drain-and-provider-write-denial")
            }
        }
        members.insert(
            "source-custody/index.json".into(),
            json_bytes(&custody).unwrap(),
        );
        reseal(&mut members);
        let changed = pack(members);
        let error = prepare(&changed, &operation(&changed))
            .err()
            .expect(mutation);
        assert!(
            error.contains(match mutation {
                "mode" => "logical saved-state",
                "count" => "count or bucket",
                "bucket" => "bucket testimony",
                "readback" => "inventory readback",
                _ => "writer fence",
            }),
            "{mutation}: {error}"
        );
    }
}

#[test]
fn older_transformations_do_not_admit_literal_null_version() {
    for transformation in [V21, V22] {
        let bytes = null_fixture();
        let mut members = unpack(&bytes).unwrap();
        let mut manifest = parse_json(&members["manifest.json"]).unwrap();
        manifest["transformation"] = json!(transformation);
        members.insert("manifest.json".into(), json_bytes(&manifest).unwrap());
        let changed = pack(members);
        assert!(
            prepare(&changed, &operation(&changed)).is_err(),
            "{transformation}"
        );
    }
}

#[test]
fn missing_or_empty_source_version_remains_refused() {
    for mutation in ["missing", "empty"] {
        let bytes = null_fixture();
        let mut members = unpack(&bytes).unwrap();
        let mut manifest = parse_json(&members["manifest.json"]).unwrap();
        let source = &mut manifest["sourceObjects"]["crdt/workspace.yjs"];
        if mutation == "missing" {
            source.as_object_mut().unwrap().remove("version_id");
        } else {
            source["version_id"] = json!("");
        }
        manifest["source"]["capture"]["snapshotBoundaries"][0]["source"] = source.clone();
        let mut descriptor = parse_json(
            manifest["captureDescriptorCanonicalJson"]
                .as_str()
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
        descriptor["snapshot_boundaries"] =
            manifest["source"]["capture"]["snapshotBoundaries"].clone();
        manifest["captureDescriptorCanonicalJson"] =
            json!(String::from_utf8(json_bytes(&descriptor).unwrap()).unwrap());
        members.insert("manifest.json".into(), json_bytes(&manifest).unwrap());
        reseal(&mut members);
        let changed = pack(members);
        assert!(
            prepare(&changed, &operation(&changed)).is_err(),
            "{mutation}"
        );
    }
}

#[test]
fn source_timestamp_adapter_preserves_native_milliseconds() {
    assert_eq!(
        checked_timestamp("2026-09-07T10:00:00+00:00").unwrap(),
        1788775200000
    );
    assert_eq!(
        checked_timestamp("2026-09-07T12:00:00+02:00").unwrap(),
        1788775200000
    );
    assert_eq!(
        checked_timestamp("2026-09-07T10:00:00.123456Z").unwrap(),
        1788775200123
    );
    assert_eq!(checked_timestamp("1970-01-01T00:00:00Z").unwrap(), 0);
    assert!(checked_timestamp("1969-12-31T23:59:59.999Z").is_err());
    assert!(checked_timestamp("1788775200000").is_err());
}

#[test]
fn actual_legacy_capture_retains_explicit_incompleteness_and_opaque_files() {
    let bytes = fs::read(
        std::env::var("GARDEN_PRESERVATION_LEGACY_ARCHIVE")
            .expect("actual legacy capture required"),
    )
    .unwrap();
    let prepared = prepare(&bytes, &operation(&bytes)).unwrap();
    assert_eq!(
        prepared.manifest["source"]["capture"]["sourceCompleteness"],
        legacy_completeness()
    );
    assert_eq!(
        prepared.manifest["source"]["capture"]["journalBoundaries"],
        json!([])
    );
    let custody = parse_json(&prepared.members["source-custody/index.json"]).unwrap();
    assert_eq!(array(&custody, "heldFiles").unwrap().len(), 3);
    // These opaque bytes are not executable Redis/WAL restore authority.
    for entry in array(&custody, "heldFiles").unwrap() {
        assert_eq!(entry["semanticRestoreAuthority"], false);
    }
    for mutate in ["complete", "source", "missing", "authority"] {
        let mut members = unpack(&bytes).unwrap();
        if mutate == "authority" {
            let mut custody = parse_json(&members["source-custody/index.json"]).unwrap();
            custody["heldFiles"][0]["semanticRestoreAuthority"] = json!(true);
            members.insert(
                "source-custody/index.json".into(),
                json_bytes(&custody).unwrap(),
            );
        } else {
            let mut manifest = parse_json(&members["manifest.json"]).unwrap();
            if mutate == "complete" {
                manifest["source"]["capture"]["sourceCompleteness"]["acknowledgedTail"] =
                    json!("complete");
            } else {
                if mutate == "source" {
                    manifest["source"]["capture"]["snapshotBoundaries"][0]["source"]["sha256"] =
                        json!("0".repeat(64));
                } else {
                    manifest["source"]["capture"]["snapshotBoundaries"]
                        .as_array_mut()
                        .unwrap()
                        .pop();
                }
                let mut descriptor = parse_json(
                    manifest["captureDescriptorCanonicalJson"]
                        .as_str()
                        .unwrap()
                        .as_bytes(),
                )
                .unwrap();
                descriptor["snapshot_boundaries"] =
                    manifest["source"]["capture"]["snapshotBoundaries"].clone();
                manifest["captureDescriptorCanonicalJson"] =
                    json!(String::from_utf8(json_bytes(&descriptor).unwrap()).unwrap());
            }
            members.insert("manifest.json".into(), json_bytes(&manifest).unwrap());
        }
        reseal(&mut members);
        let changed = pack(members);
        let error = prepare(&changed, &operation(&changed)).err().expect(mutate);
        assert!(
            error.contains(match mutate {
                "complete" => "completeness",
                "source" => "snapshot boundary",
                "missing" => "source boundaries",
                _ => "authority mismatch",
            }),
            "{mutate}: {error}"
        );
    }
}

#[test]
fn integrity_is_checked_after_outer_reseal() {
    let bytes = fixture(false);
    let mut members = unpack(&bytes).unwrap();
    let body = members
        .get_mut("originals/artifact-fixture-html/garden.html")
        .unwrap();
    body[0] ^= 1;
    let altered = pack(members);
    let error = prepare(&altered, &operation(&altered)).err().unwrap();
    assert!(error.contains("hash or length"), "{error}");
}

#[test]
fn native_original_manifest_collision_is_refused_before_effects() {
    let bytes = fixture(false);
    let mut members = unpack(&bytes).unwrap();
    let mut index = parse_json(&members["originals/index.json"]).unwrap();
    index[0]["filename"] = json!("manifest.json");
    members.insert("originals/index.json".into(), json_bytes(&index).unwrap());
    reseal(&mut members);
    let altered = pack(members);
    let error = prepare(&altered, &operation(&altered)).err().unwrap();
    // Reserved source names now receive an opaque storage key. Mutating only
    // the index must still refuse its disagreement with captured ownership.
    assert!(error.contains("workspace/RDF original metadata join mismatch"), "{error}");
}

#[test]
fn authority_graph_not_imported_even_when_hashes_are_recomputed() {
    let bytes = fixture(false);
    let mut members = unpack(&bytes).unwrap();
    let source =
        "urn:mnemosyne:user:fixture-cloud1-owner-20260908:graph:demi-cross-format-fixture-20260908";
    let mut rdf = String::from_utf8(members["rdf/dataset.nq"].clone()).unwrap();
    rdf.push_str(&format!(
        "<urn:bad> <urn:predicate> \"forged\" <{source}:projection:source> .\n"
    ));
    members.insert("rdf/dataset.nq".into(), rdf.into_bytes());
    reseal(&mut members);
    let altered = pack(members);
    let error = prepare(&altered, &operation(&altered)).err().unwrap();
    assert!(error.contains("unclassified named RDF"), "{error}");
}

#[test]
fn closed_tar_and_json_refusals() {
    assert!(parse_json(br#"{"x":{"k":1,"k":2}}"#)
        .unwrap_err()
        .contains("duplicate JSON"));
    let a = pack([("manifest.json".into(), b"{}".to_vec())]);
    let mut double = a.clone();
    double.extend_from_slice(&a);
    assert!(unpack(&double).unwrap_err().contains("multiple/trailing"));
    for names in [["same", "same"], ["same", "Same"], ["node", "node/child"]] {
        let bytes = pack(names.map(|name| (name.to_string(), Vec::new())));
        assert!(unpack(&bytes).is_err());
    }
    for path in [
        "../escape",
        "/absolute",
        "a//b",
        "a\\b",
        "a/./b",
        "moss/\u{e9}",
    ] {
        assert!(safe_path(path).is_err(), "{path}");
    }
}

#[test]
fn exclusive_custody_write_keeps_prior_bytes() {
    let root =
        std::env::temp_dir().join(format!("preservation-exclusive-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    write_new(&root, "nested/value", b"original").unwrap();
    assert!(write_new(&root, "nested/value", b"replacement").is_err());
    assert_eq!(fs::read(root.join("nested/value")).unwrap(), b"original");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(root.join("nested"), root.join("alias")).unwrap();
        assert!(write_new(&root, "alias/other", b"not admitted").is_err());
        assert!(!root.join("nested/other").exists());
    }
    eprintln!("Retained exclusive-write fixture {}", root.display());
}

fn actual_unavailable_archive() -> Vec<u8> {
    let bytes = fs::read(std::env::var("GARDEN_PRESERVATION_UNAVAILABLE_ARCHIVE")
        .expect("actual unavailable-body archive required")).unwrap();
    assert_eq!(archive_sha256(&bytes), "d5f7cfae99926e3acea0e47a252bb6db473d11e95a86000530cdb0d67b23ff6c");
    bytes
}

fn migration_history_fixture(name: &str) -> Vec<u8> {
    let (variable, digest) = match name {
        "onboarding" => ("GARDEN_PRESERVATION_ONBOARDING_ARCHIVE", "c0b2c9cea320a809b6542ba834f224c197751a9b5f771ae156a610be7921f045"),
        "vera" => ("GARDEN_PRESERVATION_VERA_ARCHIVE", "016037f7c6af16717ebf464307258d353a57f5305d721740a2f3048312b1f3d4"),
        "nixon" => ("GARDEN_PRESERVATION_NIXON_ARCHIVE", "97a1d5ec1906f765e18f7bb851ccb8bd180bb8e3ac5398d147cb4b720cfec9b6"),
        _ => panic!("unknown history fixture"),
    };
    let bytes = fs::read(std::env::var(variable).expect("retained history fixture required")).unwrap();
    assert_eq!(archive_sha256(&bytes), digest);
    bytes
}

#[test]
fn migration_history_actual_source_partition_and_negatives() {
    for (name, present, missing) in [("onboarding",4,7),("vera",56,2),("nixon",11,0)] {
        let members = unpack(&migration_history_fixture(name)).unwrap();
        let manifest = parse_json(&members["manifest.json"]).unwrap();
        let user = manifest["source"]["userId"].as_str().unwrap();
        let (unavailable, accounting) = prepare_history_availability(&members,&manifest,name,user).unwrap();
        assert_eq!(accounting["retainedMetadataCount"], present + missing);
        assert_eq!(accounting["availablePayloadCount"], present);
        assert_eq!(unavailable.len(),missing as usize);
        for row in unavailable {
            assert_eq!(row["nativeSnapshotCreated"],false);
            assert_eq!(row["semanticRestoreAuthority"],false);
            assert!(row.get("revision").is_none());
        }
        for mutation in ["omit","duplicate","foreign-owner","foreign-graph","record-hash","unknown-disposition","missing-recovery","present-as-unavailable","missing-as-present","payload-present","wrong-document","foreign-key"] {
            if missing == 0 && ["missing-as-present","payload-present"].contains(&mutation) { continue; }
            let mut changed = members.clone();
            let mut custody = parse_json(&changed["source-custody/index.json"]).unwrap();
            let rows = custody["documentHistoryRecovery"]["snapshots"].as_array_mut().unwrap();
            match mutation {
                "omit" => { rows.pop(); },
                "duplicate" => { rows.push(rows[0].clone()); },
                "record-hash" => rows[0]["recordSha256"] = json!("0".repeat(64)),
                "unknown-disposition" => rows[0]["disposition"] = json!("empty-history"),
                "present-as-unavailable" => rows.iter_mut().find(|r|r["disposition"]=="semantic-history-present").unwrap()["disposition"] = json!("custody-only-payload-unavailable"),
                "missing-as-present" => rows.iter_mut().find(|r|r["disposition"]=="custody-only-payload-unavailable").unwrap()["disposition"] = json!("semantic-history-present"),
                "payload-present" => {
                    let row = rows.iter().find(|r|r["disposition"]=="custody-only-payload-unavailable").unwrap().clone();
                    custody["objects"].as_array_mut().unwrap().push(json!({"bucket":row["bucket"],"key":row["key"],"member":"retained-existing-payload"}));
                },
                "foreign-owner" => custody["dynamoRecords"][0]["record"]["owner_user_id"]["S"] = json!("other"),
                "foreign-graph" => custody["graphId"] = json!("other"),
                "wrong-document" => rows[0]["documentId"] = json!("other"),
                "foreign-key" => rows[0]["key"] = json!("users/other/history.json"),
                _ => {
                    // Removing recovery may only pass when no metadata is missing.
                    custody.as_object_mut().unwrap().remove("documentHistoryRecovery");
                    if missing == 0 { continue; }
                },
            }
            changed.insert("source-custody/index.json".into(),json_bytes(&custody).unwrap());
            assert!(prepare_history_availability(&changed,&manifest,name,user).is_err(),"{name}: {mutation}");
        }
    }
}

#[test]
fn migration_legacy_history_corpus_partition_readback() {
    let corpus:Value=parse_json(&fs::read(std::env::var("GARDEN_PRESERVATION_HISTORY_CORPUS").unwrap()).unwrap()).unwrap();
    let output=PathBuf::from(std::env::var("GARDEN_PRESERVATION_PREPARE_OUTPUT").unwrap()).join("legacy-history-corpus");
    fs::create_dir_all(&output).unwrap();
    let mut reports=Vec::new();
    assert_eq!(corpus.as_array().unwrap().len(),19);
    let selection=std::env::var("GARDEN_PRESERVATION_PROFILE_MANIFEST").ok()
        .and_then(|path|fs::read(path).ok()).and_then(|bytes|parse_json(&bytes).ok())
        .and_then(|row|row["historyCorpusIndices"].as_array().cloned());
    let selected=selection.map(|rows|rows.iter().map(|v|v.as_u64().expect("integer corpus index") as usize).collect::<BTreeSet<_>>())
        .unwrap_or_else(||(0..19).collect());
    assert!(!selected.is_empty() && selected.iter().all(|i|*i<19));
    for (number,fixture) in corpus.as_array().unwrap().iter().enumerate() {
        if !selected.contains(&number){continue}
        let archive=fs::read(fixture["archive"].as_str().unwrap()).unwrap();
        assert_eq!(archive_sha256(&archive),fixture["archiveSha256"]);
        let members=unpack(&archive).unwrap();let manifest=parse_json(&members["manifest.json"]).unwrap();
        let graph=fixture["graphId"].as_str().unwrap();let user=fixture["sourceOwner"].as_str().unwrap();
        assert_eq!(manifest["source"]["userId"],user);assert_eq!(manifest["source"]["graphId"],graph);
        let (native,legacy)=prepare_history_partition(&members,&manifest,graph,user,true).unwrap();
        let workspace=parsed_doc(&members["crdt/workspace.yjs"],"source workspace").unwrap();
        let current=roots(&workspace,"documents").unwrap().keys().cloned().collect();
        let deletions=parse_json(&members["deletions/index.json"]).unwrap();
        let deleted=deletions.as_array().unwrap().iter().map(|row|row["documentId"].as_str().unwrap().to_string()).collect();
        let (native,legacy)=history_owner_partition(&members,&manifest,native,legacy,&current,&deleted).unwrap();
        let (unavailable,availability)=prepare_history_availability(&members,&manifest,graph,user).unwrap();
        let disposition=history_disposition(&members,&manifest,&native,&legacy,&unavailable).unwrap();
        assert_eq!(disposition["retainedMetadataCount"],availability["retainedMetadataCount"]);
        assert_eq!(native.len()+legacy.len(),manifest["counts"]["documentSnapshots"].as_u64().unwrap() as usize);
        let dir=output.join(format!("{number:02}"));fs::create_dir_all(&dir).unwrap();
        let custody=dir.join(ROOT);fs::create_dir_all(&custody).unwrap();
        let bytes=json_bytes(&disposition).unwrap();
        write_new(&custody,"document-history.json",&bytes).unwrap();
        let completion=json!({"sourceUserId":user,"sourceGraphId":graph,"documentHistoryDisposition":disposition,
            "documentHistoryDispositionFile":{"path":format!("{ROOT}/document-history.json"),"sha256":archive_sha256(&bytes),"byteLength":bytes.len()}});
        write_new(&custody,"materialization-complete.json",&json_bytes(&completion).unwrap()).unwrap();
        write_new(&custody,"source/history/documents/index.json",&members["history/documents/index.json"]).unwrap();
        for row in disposition["entries"].as_array().unwrap() {
            if let Some(path)=row["sourceMember"].as_str(){write_new(&custody,&format!("source/{path}"),&members[path]).unwrap()}
        }
        let payloads=persist_history(&dir,&native).unwrap();
        let reopened=crate::document_legacy_history::catalog(&dir,graph,user).unwrap();
        assert_eq!(reopened,disposition);
        assert!(crate::document_legacy_history::catalog(&dir,graph,"other-owner").is_err());
        assert!(crate::document_legacy_history::entry(&reopened,"../wrong","missing").is_err());
        for row in reopened["entries"].as_array().unwrap() {
            if row["status"]=="source-payload-unavailable" {
                assert_eq!(crate::document_legacy_history::source_bytes(&dir,row).unwrap_err(),"source-payload-unavailable");
            }else{
                let returned=crate::document_legacy_history::source_bytes(&dir,row).unwrap();
                assert_eq!(returned,members[row["sourceMember"].as_str().unwrap()]);
                let text=crate::document_legacy_history::literal_text(&returned).unwrap();
                assert!(text.starts_with("READ-ONLY LEGACY HISTORY"));
                if row["status"]=="legacy-read-only" {
                    assert_eq!(row["nativeSnapshotCreated"],false);assert_eq!(row["nativeRestorable"],false);
                    assert!(!payloads.iter().any(|p|p["snapshotId"]==row["snapshotId"]));
                }
                let mut changed=row.clone();changed["sourceMember"]=json!("history/documents/../../../outside");
                assert!(crate::document_legacy_history::source_bytes(&dir,&changed).is_err());
                changed=row.clone();changed["sourceMemberSha256"]=json!("0".repeat(64));
                assert!(crate::document_legacy_history::source_bytes(&dir,&changed).is_err());
                changed=row.clone();changed["sourceUserId"]=json!("forged-owner");
                assert!(crate::document_legacy_history::source_bytes(&dir,&changed).is_err());
            }
        }
        let mut wrong=manifest.clone();wrong["sourceRecords"][0]["record_sha256"]=json!("0".repeat(64));
        assert!(prepare_history_partition(&members,&wrong,graph,user,true).is_err());
        assert!(prepare_history_partition(&members,&manifest,graph,"other-owner",true).is_err());
        reports.push(json!({"input":fixture,"disposition":disposition,"historyOnlyGraphDirectory":dir,
            "nativePayloads":payloads,"physicalReadback":true,"httpReadback":false,"freshProcessReadback":false}));
    }
    write_new(&output,"result.json",&json_bytes(&reports).unwrap()).unwrap();
}

#[test]
fn migration_legacy_fidelity_never_conceals_unavailable_or_empty_history() {
    assert_eq!(history_fidelity(0,0,0),"no-source-history");
    assert_eq!(history_fidelity(4,0,0),"qualified-native-interpretation");
    assert_eq!(history_fidelity(0,0,7),"source-history-unavailable");
    assert_eq!(history_fidelity(4,0,7),"disclosed-unavailable-history");
    assert_eq!(history_fidelity(0,2,0),"disclosed-read-only-legacy-history");
    assert_eq!(history_fidelity(4,2,7),"disclosed-legacy-and-unavailable-history");
}

#[test]
fn migration_history_null_image_mime_preserves_source_absence() {
    let members = unpack(&migration_history_fixture("vera")).unwrap();
    let manifest = parse_json(&members["manifest.json"]).unwrap();
    let originals = parse_json(&members["originals/index.json"]).unwrap();
    let mut nulls = 0;
    for original in originals.as_array().unwrap() {
        let mime = original_effective_mime(original,&manifest).unwrap();
        if original["mimeType"].is_null() {
            nulls += 1;
            assert_eq!(mime,"application/octet-stream");
            let mut changed = original.clone();
            changed["ownerKind"] = json!("document");
            assert!(original_effective_mime(&changed,&manifest).is_err());
            changed = original.clone(); changed.as_object_mut().unwrap().remove("mimeType");
            assert!(original_effective_mime(&changed,&manifest).is_err());
            changed = original.clone(); changed["mimeType"] = json!(false);
            assert!(original_effective_mime(&changed,&manifest).is_err());
            let mut older = manifest.clone(); older["transformation"] = json!(V22);
            assert!(original_effective_mime(original,&older).is_err());
            // The exact original member remains physically retrievable through
            // the standard original-file storage with explicit native MIME.
            let root = std::env::temp_dir().join(format!("null-image-mime-{}",uuid::Uuid::new_v4()));
            let bytes = member(&members,original["member"].as_str().unwrap()).unwrap();
            crate::original_file_storage::save_original_bytes_to_dir(&root,original["filename"].as_str().unwrap(),mime,bytes).unwrap();
            let (stored,returned) = crate::original_file_storage::read_original_file_from_dir(&root).unwrap();
            assert_eq!(stored.mime_type,mime); assert_eq!(returned,bytes);
        } else { assert_eq!(mime,original["mimeType"]); }
    }
    assert_eq!(nulls,2);
}

fn migration_history_full_prepare(name: &str, present: usize, unavailable: usize, originals: usize) {
    let bytes = migration_history_fixture(name);
    let prepared = prepare(&bytes,&manifest_operation(&bytes)).unwrap();
    assert_eq!(prepared.history.len(),present);
    assert_eq!(prepared.unavailable_history.len(),unavailable);
    assert_eq!(prepared.originals.len(),originals);
    assert_eq!(prepared.members,unpack(&bytes).unwrap());
    let native_pairs: BTreeSet<_> = prepared.history.iter().map(|item|item.meta.snapshot_id.as_str()).collect();
    assert!(prepared.unavailable_history.iter().all(|row| !native_pairs.contains(row["snapshotId"].as_str().unwrap())));
    let storage = std::env::temp_dir().join(format!("history-availability-{}",uuid::Uuid::new_v4()));
    fs::create_dir(&storage).unwrap();
    let payloads = persist_history(&storage,&prepared.history).unwrap();
    assert_eq!(payloads.len(),present);
    for item in &prepared.history {
        let returned = crate::document_history_file_store::read_document_snapshot_payload(
            &storage,&item.meta.document_id,&item.meta.snapshot_id).unwrap();
        assert_eq!(json_bytes(&returned).unwrap(),json_bytes(&item.payload).unwrap());
    }
    for row in &prepared.unavailable_history {
        let doc = row["documentId"].as_str().unwrap();
        let id = row["snapshotId"].as_str().unwrap();
        let returned = crate::document_history_file_store::read_document_snapshot_payload(&storage,doc,id);
        assert_eq!(returned.unwrap_err(),format!("snapshot not found: {id}"));
        let store = crate::document_history_file_store::read_document_history_store(&storage,name,doc).unwrap();
        assert!(!store.snapshots.iter().any(|entry|entry.snapshot_id == id));
    }
    emit_actual_prepared_fixture(&prepared,&bytes,name);
}

#[test]
fn migration_history_onboarding_full_prepare() { migration_history_full_prepare("onboarding",4,7,0); }

#[test]
fn migration_wire_endpoints_require_saved_fields_scope_and_exact_native_projection() {
    use oxigraph::model::{Quad,NamedNode,Literal,GraphName};
    let source_user="source-user"; let source_graph="source-graph"; let target="target-graph";
    let snapshot=json!({"documents":[],"folders":[],"artifacts":[],"wires":[{
        "id":"wire-one","sourceDocumentId":"absent-source","sourceBlockId":"absent-block",
        "targetDocumentId":"absent-target","targetBlockId":"target-block","targetGraphId":"other-graph"}]});
    let projected_text=crate::rdf_workspace_materializer::workspace_entity_triples(target,&snapshot)
        .iter().map(crate::rdf::format_rdf_triple).collect::<Vec<_>>().join("\n");
    let projected=RdfParser::from_format(RdfFormat::NTriples).for_slice(projected_text.as_bytes())
        .map(|q|{let q=q.unwrap();format!("{} {} {}",q.subject,q.predicate,q.object)}).collect::<std::collections::HashSet<_>>();
    assert!(!projected.iter().any(|s|s.starts_with("<urn:mnemosyne:local:document:")));
    for (predicate,graph,doc,suffix) in [
        ("sourceDocument",source_graph,"absent-source",""),
        ("sourceBlock",source_graph,"absent-source","#block-absent-block"),
        ("targetDocument","other-graph","absent-target",""),
        ("targetBlock","other-graph","absent-target","#block-target-block"),
    ] {
        let quad=Quad::new(NamedNode::new(format!("{}:wire:wire-one",crate::rdf::graph_subject(target))).unwrap(),
            NamedNode::new(format!("{}{predicate}",crate::runtime_config::WIRE_NS)).unwrap(),
            NamedNode::new(format!("urn:mnemosyne:user:{source_user}:graph:{graph}:doc:{doc}{suffix}")).unwrap(),GraphName::DefaultGraph);
        let evidence=legacy_saved_wire_endpoint(&quad,&snapshot,&projected,source_user,source_graph,target).unwrap();
        assert_eq!(evidence["referenceOnly"],true);assert_eq!(evidence["endpointExistenceAsserted"],false);
        assert_eq!(evidence["fetchAuthority"],false);assert_eq!(evidence["endpointGraphId"],graph);
        assert!(legacy_saved_wire_endpoint(&quad,&snapshot,&std::collections::HashSet::new(),source_user,source_graph,target).is_none());
        for mutation in ["owner","graph","document","predicate","subject","literal","workspace","duplicate","block"] {
            let mut q=quad.clone();let mut state=snapshot.clone();
            match mutation {
                "owner" => q.object=NamedNode::new(format!("urn:mnemosyne:user:other:graph:{graph}:doc:{doc}{suffix}")).unwrap().into(),
                "graph" => q.object=NamedNode::new(format!("urn:mnemosyne:user:{source_user}:graph:wrong:doc:{doc}{suffix}")).unwrap().into(),
                "document" => q.object=NamedNode::new(format!("urn:mnemosyne:user:{source_user}:graph:{graph}:doc:wrong{suffix}")).unwrap().into(),
                "predicate" => q.predicate=NamedNode::new("urn:custom:authored").unwrap(),
                "subject" => q.subject=NamedNode::new("urn:custom:authored-subject").unwrap().into(),
                "literal" => q.object=Literal::new_simple_literal("not-an-IRI").into(),
                "workspace" => state["wires"]=json!([]),
                "duplicate" => {let row=state["wires"][0].clone();state["wires"].as_array_mut().unwrap().push(row);},
                _ => { if suffix.is_empty(){continue;} q.object=NamedNode::new(format!("urn:mnemosyne:user:{source_user}:graph:{graph}:doc:{doc}#block-other")).unwrap().into(); },
            }
            assert!(legacy_saved_wire_endpoint(&q,&state,&projected,source_user,source_graph,target).is_none(),"{predicate}: {mutation}");
        }
    }
}

#[test]
fn migration_inline_atoms_onboarding_history_persistence_and_text_projection() {
    let bytes = migration_history_fixture("onboarding");
    let members = unpack(&bytes).unwrap();
    let manifest = parse_json(&members["manifest.json"]).unwrap();
    let user = manifest["source"]["userId"].as_str().unwrap();
    let history = prepare_history(&members,&manifest,"onboarding",user).unwrap();
    let (missing,availability) = prepare_history_availability(&members,&manifest,"onboarding",user).unwrap();
    assert_eq!(history.len(),4); assert_eq!(missing.len(),7);
    let root = PathBuf::from(std::env::var("GARDEN_PRESERVATION_PREPARE_OUTPUT").unwrap())
        .join("onboarding-history-only");
    fs::create_dir(&root).unwrap();
    let native = root.join("native"); fs::create_dir(&native).unwrap();
    let payloads = persist_history(&native,&history).unwrap();
    let mut projections = Vec::new();
    let mut wikilink_count = 0;
    let mut native_wikilink_marks = 0;
    for item in &history {
        let returned = crate::document_history_file_store::read_document_snapshot_payload(
            &native,&item.meta.document_id,&item.meta.snapshot_id).unwrap();
        assert_eq!(json_bytes(&returned).unwrap(),json_bytes(&item.payload).unwrap());
        wikilink_count += returned.tiptap_xml.matches("<wikilink ").count();
        native_wikilink_marks += returned.blocks.iter().flat_map(|b|&b.marks).filter(|mark|mark.mark_type == "wikilink").count();
        // Same pure formatter used by hosted_document_snapshot_text_response;
        // this is a physical file/function witness, not an HTTP call.
        let text = crate::document_history_projection::snapshot_blocks_markdown(&returned.blocks);
        let path = format!("{}.text.txt",item.meta.snapshot_id);
        fs::write(root.join(&path),text.as_bytes()).unwrap();
        projections.push(json!({"snapshotId":item.meta.snapshot_id,"documentId":item.meta.document_id,
            "path":path,"sha256":archive_sha256(text.as_bytes()),"byteLength":text.len()}));
    }
    assert!(wikilink_count > 0);
    assert_eq!(native_wikilink_marks,wikilink_count);
    for row in &missing {
        let id = row["snapshotId"].as_str().unwrap(); let doc = row["documentId"].as_str().unwrap();
        assert_eq!(crate::document_history_file_store::read_document_snapshot_payload(&native,doc,id)
            .unwrap_err(),format!("snapshot not found: {id}"));
        let store = crate::document_history_file_store::read_document_history_store(&native,"onboarding",doc).unwrap();
        assert!(!store.snapshots.iter().any(|item|item.snapshot_id == id));
    }
    let result = json!({"qualification":"history-only-private-file-and-formatter-witness-not-full-graph-or-http",
        "archiveSha256":archive_sha256(&bytes),"sourceUserId":user,"sourceGraphId":"onboarding",
        "documentHistoryPayloads":payloads,"unavailableDocumentHistory":missing,
        "documentHistoryAvailability":availability,"nativePublicTextProjections":projections,
        "wikilinkCount":wikilink_count,"nativeWikilinkMarkCount":native_wikilink_marks,
        "nativeTextEqualsLegacyFlatText":"not-asserted-native-displays-validated-atoms"});
    fs::write(root.join("result.json"),json_bytes(&result).unwrap()).unwrap();
}

#[test]
fn migration_inline_atoms_exact_frames_and_source_text() {
    for atom in [
        "<wikilink label=\"Visible\" targetDocId=\"target\" targetGraphId=\"graph\" wireId=\"wire\"></wikilink>",
        "<tagChip name=\"todo\" date=\"2026-09-12\"></tagChip>",
        "<footnote content=\"A &amp; B\"></footnote>",
    ] {
        let xml = format!("<paragraph data-block-id=\"p\">before <literal> {atom} after</paragraph>");
        let rows = vec![json!({"id":"p","type":"paragraph","text":"before <literal>  after",
            "parent_id":null,"index":0,"order":0,"collapsed":false})];
        let interpreted = legacy_history_export_xml(&xml,&rows).unwrap();
        assert!(interpreted.contains(atom));
        assert!(interpreted.contains("&lt;literal&gt;"));
        let parsed = super::super::super::content_parse::parse_write_content_for_operation(&interpreted,Some("xml"),"atom-fixture").unwrap();
        assert!(parsed.warnings.is_empty());
        validate_legacy_history_blocks(&interpreted,&rows,&parsed.tiptap_json).unwrap();
        let mut changed = parsed.tiptap_json.clone();
        let inline = changed["content"][0]["content"].as_array_mut().unwrap().iter_mut()
            .find(|n|n["type"] != "text").unwrap();
        inline["attrs"]["unexpected"] = json!("not-source");
        assert!(validate_legacy_history_blocks(&interpreted,&rows,&changed).is_err());
        let literal_rows = vec![json!({"id":"p","type":"paragraph","text":format!("before {atom} after"),
            "parent_id":null,"index":0,"order":0,"collapsed":false})];
        let literal_xml = format!("<paragraph data-block-id=\"p\">before {atom} after</paragraph>");
        let literal = legacy_history_export_xml(&literal_xml,&literal_rows).unwrap();
        assert!(!literal.contains(atom));
        assert!(literal.contains("&lt;"));
    }
}

#[test]
fn migration_inline_atoms_refuse_ambiguous_or_changed_frames() {
    let rows = vec![json!({"id":"p","type":"paragraph","text":"before  after",
        "parent_id":null,"index":0,"order":0,"collapsed":false})];
    for xml in [
        "<paragraph data-block-id=\"other\">before <tagChip name=\"todo\"></tagChip> after</paragraph>",
        "<heading data-block-id=\"p\">before <tagChip name=\"todo\"></tagChip> after</heading>",
        "<paragraph data-block-id=\"p\">different <tagChip name=\"todo\"></tagChip> after</paragraph>",
        "<paragraph data-block-id=\"p\">before <tagChip name=\"todo\" unknown=\"x\"></tagChip> after</paragraph>",
        "<paragraph data-block-id=\"p\">before <tagChip name=\"todo\">lost text</tagChip> after</paragraph>",
        "<paragraph data-block-id=\"p\">before <footnote content=\"unescaped <angle>\"></footnote> after</paragraph>",
    ] { assert!(legacy_history_export_xml(xml,&rows).is_err()); }
    let frame = "<paragraph data-block-id=\"p\">before <tagChip name=\"todo\"></tagChip> after</paragraph>";
    assert!(legacy_history_export_xml(&format!("{frame}{frame}"),&rows).is_err());
    let wrapped = "<paragraph data-block-id=\"p\"><strong>before <tagChip name=\"todo\"></tagChip> after</strong></paragraph>";
    let marked = vec![json!({"id":"p","type":"paragraph","text":"<strong>before  after</strong>",
        "parent_id":null,"index":0,"order":0,"collapsed":false})];
    let interpreted = legacy_history_export_xml(wrapped,&marked).unwrap();
    let parsed = super::super::super::content_parse::parse_write_content_for_operation(&interpreted,Some("xml"),"marked-atom").unwrap();
    assert!(validate_legacy_history_blocks(&interpreted,&marked,&parsed.tiptap_json).is_err());
}

#[test]
fn migration_inline_atoms_full_link_attributes_are_independently_checked() {
    let text = "before <link class=\"external\" href=\"https://example.invalid/\" rel=\"noopener noreferrer\" target=\"_blank\">label</link> after";
    let rows = vec![json!({"id":"p","type":"paragraph","text":text,
        "parent_id":null,"index":0,"order":0,"collapsed":false})];
    let xml = format!("<paragraph data-block-id=\"p\">{text}</paragraph>");
    let interpreted = legacy_history_export_xml(&xml,&rows).unwrap();
    let parsed = super::super::super::content_parse::parse_write_content_for_operation(&interpreted,Some("xml"),"link-attrs").unwrap();
    validate_legacy_history_blocks(&interpreted,&rows,&parsed.tiptap_json).unwrap();
    for attr in ["class","href","rel","target"] {
        let mut changed = parsed.tiptap_json.clone();
        let marked = changed["content"][0]["content"].as_array_mut().unwrap().iter_mut()
            .find(|node|node.get("marks").is_some()).unwrap();
        marked["marks"][0]["attrs"].as_object_mut().unwrap().remove(attr);
        assert!(validate_legacy_history_blocks(&interpreted,&rows,&changed).is_err());
    }
    let bad_text = text.replace("class=", "onclick=");
    let bad_rows = vec![json!({"id":"p","type":"paragraph","text":bad_text,
        "parent_id":null,"index":0,"order":0,"collapsed":false})];
    let bad_xml = format!("<paragraph data-block-id=\"p\">{bad_text}</paragraph>");
    let bad = legacy_history_export_xml(&bad_xml,&bad_rows).unwrap();
    let parsed = super::super::super::content_parse::parse_write_content_for_operation(&bad,Some("xml"),"bad-attrs").unwrap();
    assert!(validate_legacy_history_blocks(&bad,&bad_rows,&parsed.tiptap_json).is_err());
}
#[test]
fn migration_history_vera_full_prepare() { migration_history_full_prepare("vera",56,2,3); }
#[test]
fn migration_history_nixon_originals_full_prepare() { migration_history_full_prepare("nixon",11,0,2); }

#[test]
fn unavailable_original_reference_is_inert_and_requires_saved_and_native_absence() {
    use oxigraph::model::{Quad,NamedNode,Literal,GraphName};
    let subject=NamedNode::new("urn:synthetic:document").unwrap();
    for key in ["sourceStorageKey","sourceOriginalFilename","sourceMimeType","sourceFileType","sourceContentSize"] {
        let predicate=NamedNode::new(format!("http://mnemosyne.dev/doc#{key}")).unwrap();
        let literal=if key=="sourceContentSize" { Literal::new_typed_literal("14302.0",NamedNode::new("http://www.w3.org/2001/XMLSchema#integer").unwrap()) } else { Literal::new_simple_literal("retained-reference") };
        let quad=Quad::new(subject.clone(),predicate.clone(),literal,GraphName::DefaultGraph);
        let authority=HashMap::from([(subject.as_str().to_string(),json!({key:{"root":"documents","entityId":"synthetic","key":key,"presence":"absent","value":null}}))]);
        let empty=std::collections::HashSet::new();
        let result=legacy_absent_original_reference(&quad,&empty,&authority).unwrap();
        assert!(result["native"].is_null());assert_eq!(result["fetchAuthority"],false);
        assert_eq!(result["originalAvailability"],"not-established");
        let projected=[format!("{subject} {predicate} {}",Literal::new_simple_literal("current"))].into_iter().collect();
        assert!(legacy_absent_original_reference(&quad,&projected,&authority).is_none());
        for presence in ["present","null"] {
            let mut changed=authority.clone();changed.get_mut(subject.as_str()).unwrap()[key]["presence"]=json!(presence);
            assert!(legacy_absent_original_reference(&quad,&empty,&changed).is_none());
        }
        let mut changed=quad.clone();changed.object=NamedNode::new("urn:do-not-fetch").unwrap().into();
        assert!(legacy_absent_original_reference(&changed,&empty,&authority).is_none());
        changed=quad.clone();changed.predicate=NamedNode::new("http://mnemosyne.dev/doc#readOnly").unwrap();
        assert!(legacy_absent_original_reference(&changed,&empty,&authority).is_none());
    }
}

#[test]
fn actual_unavailable_bodies_preserve_metadata_without_fabricating_updates() {
    let bytes = actual_unavailable_archive();
    let source = unpack(&bytes).unwrap();
    let prepared = prepare(&bytes, &manifest_operation(&bytes)).unwrap();
    assert!(prepared.members == source);
    assert_eq!(prepared.parsed.documents.len(), 27);
    assert_eq!(prepared.unavailable_bodies.len(), 4);
    assert_eq!(prepared.snapshot["documents"].as_array().unwrap().len(), 31);
    for row in &prepared.unavailable_bodies {
        let id = row["documentId"].as_str().unwrap();
        assert!(!prepared.parsed.documents.iter().any(|(other,_)| other == id));
        assert!(!source.contains_key(&format!("crdt/documents/{id}.yjs")));
        assert!(prepared.snapshot["documents"].as_array().unwrap().iter().any(|d| d["id"] == id));
    }
    for (id, body) in &prepared.parsed.documents {
        assert!(body == &source[&format!("crdt/documents/{id}.yjs")]);
    }
}

#[test]
fn unavailable_body_partition_and_source_negatives() {
    for mutation in ["foreign-owner", "foreign-graph", "unknown-id", "reason", "inventory", "omit", "duplicate", "present"] {
        let bytes = actual_unavailable_archive();
        let mut members = unpack(&bytes).unwrap();
        let mut custody = parse_json(&members["source-custody/index.json"]).unwrap();
        match mutation {
            "foreign-owner" => custody["unavailableBodies"][0]["sourceUserId"] = json!("other"),
            "foreign-graph" => custody["unavailableBodies"][0]["sourceGraphId"] = json!("other"),
            "unknown-id" => custody["unavailableBodies"][0]["documentId"] = json!("other"),
            "reason" => custody["unavailableBodies"][0]["reason"] = json!("empty"),
            "inventory" => custody["unavailableBodies"][0]["inventorySha256"] = json!("0".repeat(64)),
            "omit" => { custody["unavailableBodies"].as_array_mut().unwrap().pop(); },
            "duplicate" => { let row = custody["unavailableBodies"][0].clone(); custody["unavailableBodies"].as_array_mut().unwrap().push(row); },
            _ => {
                let name = members.keys().find(|n| n.starts_with("crdt/documents/")).unwrap();
                custody["unavailableBodies"][0]["documentId"] = json!(name.trim_start_matches("crdt/documents/").trim_end_matches(".yjs"));
            }
        }
        members.insert("source-custody/index.json".into(), json_bytes(&custody).unwrap());
        reseal(&mut members);
        let changed = pack(members);
        assert!(prepare(&changed, &manifest_operation(&changed)).is_err(), "accepted {mutation}");
    }
}

#[test]
fn retained_owner_catalogue_partition_has_no_native_catalogue_authority() {
    let bytes=include_bytes!("../../tests/fixtures/retained-dataset/owner-meta-v1.tar.gz");
    let prepared=prepare(bytes,&manifest_operation(bytes)).expect("owner metadata capture");
    let rows=prepared.dataset_partitions["partitions"].as_array().unwrap();
    let row=rows.iter().find(|r|r["identity"]["iri"].as_str().is_some_and(|s|s.ends_with(":meta"))).unwrap();
    assert_eq!(row["disposition"],"retained-source-testimony");
    assert_ne!(row["destinationGraph"],format!("urn:mnemosyne:local:graph:{}",prepared.manifest["source"]["graphId"].as_str().unwrap()));
    assert!(prepared.rdf.contains("Historical metadata"));
}

#[test]
fn bounded_gnu_long_names_preserve_compliant_ids_and_reject_ambiguous_headers() {
    let path=format!("crdt/documents/{}.yjs", "a".repeat(110));
    let encoder=GzEncoder::new(Vec::new(),Compression::fast());
    let mut builder=tar::Builder::new(encoder);
    let mut header=tar::Header::new_gnu();
    header.set_size(4);header.set_mode(0o600);header.set_mtime(0);header.set_cksum();
    builder.append_data(&mut header,&path,&b"data"[..]).unwrap();
    let archive=builder.into_inner().unwrap().finish().unwrap();
    assert_eq!(unpack(&archive).unwrap().get(&path).unwrap(),b"data");
    let mut raw=Vec::new();flate2::read::GzDecoder::new(archive.as_slice()).read_to_end(&mut raw).unwrap();
    let compress=|value:&[u8]| {let mut gz=GzEncoder::new(Vec::new(),Compression::fast());gz.write_all(value).unwrap();gz.finish().unwrap()};
    let mut traversal=raw.clone();traversal[512..515].copy_from_slice(b"../");
    assert!(unpack(&compress(&traversal)).is_err());
    let mut repeated=raw[..1024].to_vec();repeated.extend_from_slice(&raw);
    assert!(unpack(&compress(&repeated)).is_err());
    let mut orphan=raw[..1024].to_vec();orphan.extend_from_slice(&[0;1024]);
    assert!(unpack(&compress(&orphan)).is_err());
    let mut mismatch=raw.clone();mismatch[1024]=b'z';
    mismatch[1024+148..1024+156].fill(b' ');
    let sum:usize=mismatch[1024..1536].iter().map(|b|*b as usize).sum();
    mismatch[1024+148..1024+156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    assert!(unpack(&compress(&mismatch)).is_err());
}

#[test]
fn derived_dataset_is_exactly_declared_scoped_identifiers_and_metadata() {
    let workspace=Doc::new();workspace.get_or_insert_map("wires");
    let mut members=Members::new();members.insert("crdt/workspace.yjs".into(),workspace.transact().encode_state_as_update_v1(&yrs::StateVector::default()));
    let manifest=json!({"compatibilityTransform":{"rule":"vera-20260920-standard-compatibility-v1",
        "originalCustodyUnchanged":true,"identifierMap":{"old.name":"new-name"},"wireMetadataDefaults":[]}});
    let raw=b"<urn:mnemosyne:user:owner:graph:g:doc:old.name> <urn:p> \"body\" <urn:mnemosyne:user:owner:graph:g> .\n<urn:mnemosyne:user:other:graph:g:doc:old.name> <urn:p> \"foreign reference\" <urn:mnemosyne:user:owner:graph:g> .\n";
    let changed=String::from_utf8(raw.to_vec()).unwrap().replacen(":doc:old.name>",":doc:new-name>",1);
    let verify=|value:&str|derived_dataset::verify_derived_dataset(raw,value.as_bytes(),&members,&manifest,"owner","g");
    verify(&changed).unwrap();
    assert!(verify(&changed.replace("body","edited")).is_err());
    assert!(verify(&changed.replace("other:graph:g:doc:old.name","other:graph:g:doc:new-name")).is_err());
    assert!(verify("").is_err());
    assert!(verify(&format!("{changed}{changed}")).is_err());
    let mut manifest=manifest.clone();
    manifest["compatibilityTransform"]["wireMetadataDefaults"]=json!([{
        "subject":"urn:mnemosyne:user:owner:graph:g:wire:w","predicate":"http://mnemosyne.ai/vocab#bidirectional",
        "value":"\"false\"^^<http://www.w3.org/2001/XMLSchema#boolean>","sourceValue":null,
        "rule":"legacy-wire-metadata-default-v1","referenceOnly":true,"currentEntityExistenceAsserted":false}]);
    let semantic=format!("{changed}<urn:mnemosyne:user:owner:graph:g:wire:w> <http://mnemosyne.ai/vocab#bidirectional> \"false\"^^<http://www.w3.org/2001/XMLSchema#boolean> <urn:mnemosyne:user:owner:graph:g> .\n");
    derived_dataset::verify_derived_dataset(raw,semantic.as_bytes(),&members,&manifest,"owner","g").unwrap();
    manifest["compatibilityTransform"]["wireMetadataDefaults"][0]["currentEntityExistenceAsserted"]=json!(true);
    assert!(derived_dataset::verify_derived_dataset(raw,semantic.as_bytes(),&members,&manifest,"owner","g").is_err());
}


#[test]
fn historical_identifier_mapping_keeps_raw_metadata_and_rejects_unrelated_drift() {
    let manifest=json!({"compatibilityTransform":{"rule":"vera-20260920-standard-compatibility-v1",
        "originalCustodyUnchanged":true,"identifierMap":{"old.name":"new-name"}}});
    let record=json!({"doc_id":{"S":"old.name"},"doc_key":{"S":"g#old.name"},
        "graph_id":{"S":"g"},"owner_user_id":{"S":"owner"},"snapshot_id":{"S":"s"}});
    let entry=json!({"doc_id":"new-name","doc_key":"g#new-name","graph_id":"g",
        "owner_user_id":"owner","snapshot_id":"s","member":"history/documents/s.json"});
    let retained=record.clone();
    check_history_record(&manifest,"g",&entry,&record).unwrap();
    assert_eq!(record,retained);
    for key in ["doc_id","doc_key","graph_id","owner_user_id","snapshot_id"] {
        let mut bad=entry.clone();bad[key]=json!("foreign");
        assert!(check_history_record(&manifest,"g",&bad,&record).is_err());
    }
    assert!(check_history_record(&json!({}),"g",&entry,&record).is_err());
    let mut bad=manifest.clone();bad["compatibilityTransform"]["originalCustodyUnchanged"]=json!(false);
    assert!(check_history_record(&bad,"g",&entry,&record).is_err());
    assert!(check_history_record(&manifest,"other",&entry,&record).is_err());
    let mut bad=record.clone();bad["doc_key"]=json!({"S":"other#old.name"});
    assert!(check_history_record(&manifest,"g",&entry,&bad).is_err());
}

#[test]
fn historical_projection_shares_identical_versions_with_exact_payload_fidelity() {
    let members=unpack(include_bytes!("../../tests/fixtures/retained-dataset/retained-dataset-v1.tar.gz")).unwrap();
    let (path,bytes)=members.iter().find(|(p,_)|p.starts_with("crdt/documents/")).unwrap();
    let id=path.strip_prefix("crdt/documents/").unwrap().strip_suffix(".yjs").unwrap();
    let mut cache=HistoryProjectionCache::new();
    let content=history_projection(&mut cache,bytes,id).unwrap();
    let repeated=history_projection(&mut cache,bytes,id).unwrap();
    assert!(Arc::ptr_eq(&content,&repeated));
    assert_eq!(cache.len(),1);
    assert_eq!(content.bytes.as_ref(),bytes.as_slice());
    let doc=parsed_doc(bytes,"fixture").unwrap();
    let projection=super::super::super::projection::materialize_ydoc(&doc,id);
    let payload=GraphHistoryPayload {snapshot_id:"s".into(),graph_id:"g".into(),document_id:id.into(),
        title:"Preserved title".into(),created_at:"1789150000000".into(),content:Arc::clone(&content)};
    let expected=LocalDocumentSnapshotPayload {snapshot_id:"s".into(),graph_id:"g".into(),document_id:id.into(),
        title:"Preserved title".into(),created_at:"1789150000000".into(),
        blocks:serde_json::from_value(projection.blocks_json).unwrap(),
        tiptap_xml:super::super::super::projection::ydoc_to_tiptap_xml(&doc)};
    // Independent projections generate fresh native mark IDs and may enumerate
    // equivalent marks differently. These IDs are not historical source IDs.
    // Preserve every authored block ID, mark kind/range/attribute and XML byte.
    let canonical = |value: LocalDocumentSnapshotPayload| {
        let mut value = serde_json::to_value(value).unwrap();
        for block in value["blocks"].as_array_mut().unwrap() {
            let marks = block["marks"].as_array_mut().unwrap();
            for mark in marks.iter_mut() { mark.as_object_mut().unwrap().remove("id"); }
            marks.sort_by_key(|mark| serde_json::to_string(mark).unwrap());
        }
        value
    };
    assert_eq!(canonical(payload.native()),canonical(expected));
    // Reusing the cached projection must itself remain byte-stable, including
    // its chosen generated IDs. Serialization must not regenerate any content.
    assert_eq!(json_bytes(&payload.native()).unwrap(),json_bytes(&payload.native()).unwrap());
    assert_eq!(content.char_count,projection.body.chars().count() as u64);
    assert!(!Arc::ptr_eq(&content,&history_projection(&mut cache,bytes,"different-id").unwrap()));
    let empty=Doc::new().transact().encode_state_as_update_v1(&yrs::StateVector::default());
    assert!(!Arc::ptr_eq(&content,&history_projection(&mut cache,&empty,id).unwrap()));
    assert!(history_projection(&mut cache,b"invalid update",id).is_err());
    assert_eq!(cache.len(),3);
    cache.insert((id.into(),archive_sha256(bytes)),Arc::clone(&history_projection(&mut HistoryProjectionCache::new(),&empty,id).unwrap()));
    assert!(history_projection(&mut cache,bytes,id).is_err());
}

#[test]
fn original_rdf_index_preserves_scope_multiplicity_and_rejects_malformed_tail() {
    let graph = "urn:mnemosyne:user:owner:graph:graph";
    let row = |subject: &str, predicate: &str, object: &str, scope: &str| {
        format!("<{subject}> <http://mnemosyne.dev/doc#{predicate}> {object} <{scope}> .\n")
    };
    let document = |id: &str| {
        let subject = format!("{graph}:doc:{id}");
        [("sourceStorageKey", "\"key\""), ("sourceOriginalFilename", "\"file.pdf\""),
         ("sourceMimeType", "\"application/pdf\""), ("sourceFileType", "\"pdf\""),
         ("sourceContentSize", "\"123\"^^<http://www.w3.org/2001/XMLSchema#integer>")]
            .iter().map(|(predicate, object)| row(&subject, predicate, object, graph)).collect::<String>()
    };
    let subject = format!("{graph}:doc:one");
    let artifact = format!("{graph}:artifact:one");
    let mut rdf = document("one") + &document("two");
    rdf += &row(&subject, "sourceStorageKey", "\"foreign\"", "urn:foreign");
    rdf += &row("urn:foreign:doc:one", "sourceStorageKey", "\"foreign\"", graph);
    rdf += &row(&subject, "unrelated", "<urn:nonliteral>", graph);
    rdf += &row(&artifact, "storageKey", "\"artifact-key\"", graph);
    let index = OriginalRdfIndex::parse(&rdf, "owner", "graph").unwrap();
    assert_eq!(index.subjects.len(), 3);
    assert_eq!(index.rows(&subject).len(), 5);
    assert_eq!(index.rows(&artifact).len(), 1);
    assert!(index.rows("urn:foreign:doc:one").is_empty());
    let expected = RdfOriginal { document_id: "one", storage_key: "key", filename: "file.pdf",
        mime: "application/pdf", file_type: "pdf", byte_length: 123 };
    for id in ["one", "two"] {
        assert!(indexed_rdf_original_matches_with_evidence(&index, "owner", "graph",
            &RdfOriginal { document_id: id, ..expected }, None).unwrap());
    }
    for (owner, graph) in [("other", "graph"), ("owner", "other")] {
        assert!(!indexed_rdf_original_matches_with_evidence(&index, owner, graph, &expected, None).unwrap());
    }
    for extra in [row(&subject, "sourceStorageKey", "\"key\"", graph),
                  row(&subject, "sourceMimeType", "<urn:nonliteral>", graph)] {
        let changed = OriginalRdfIndex::parse(&(rdf.clone() + &extra), "owner", "graph").unwrap();
        assert!(!indexed_rdf_original_matches_with_evidence(&changed, "owner", "graph", &expected, None).unwrap());
        assert!(indexed_rdf_original_matches_with_evidence(&changed, "owner", "graph",
            &RdfOriginal { document_id: "two", ..expected }, None).unwrap());
    }
    assert!(OriginalRdfIndex::parse(&(rdf + "malformed tail"), "owner", "graph").is_err());
}

#[test]
fn indexed_legacy_history_preserves_large_document_and_rejects_ambiguous_ids() {
    let mut xml = String::new();
    let mut rows = Vec::new();
    for i in 0..1024 {
        let id = format!("history-index-{i}");
        let text = format!("Row {i}: <literal> & exact text");
        xml.push_str(&format!("<paragraph data-block-id=\"{id}\">{text}</paragraph>"));
        rows.push(json!({"id":id,"type":"paragraph","text":text,"parent_id":null,
            "index":i,"order":i,"collapsed":false}));
    }
    let encoded = legacy_history_export_xml(&xml, &rows).unwrap();
    let parsed = crate::crdt_engine::content_parse::parse_write_content_for_operation(
        &encoded, Some("xml"), "history-index-test").unwrap();
    assert!(parsed.warnings.is_empty());
    validate_legacy_history_blocks(&encoded, &rows, &parsed.tiptap_json).unwrap();
    assert_eq!(parsed.tiptap_json["content"].as_array().unwrap().len(), 1024);
    let projection = crate::crdt_engine::projection::materialize_tiptap_json(&parsed.tiptap_json,"history-index-test");
    for (i, block) in projection.blocks_json.as_array().unwrap().iter().enumerate() {
        assert_eq!(block["id"], rows[i]["id"]);
        assert_eq!(block["content"], rows[i]["text"]);
    }
    let mut duplicate_native = parsed.tiptap_json.clone();
    let duplicate = duplicate_native["content"][512].clone();
    duplicate_native["content"].as_array_mut().unwrap().push(duplicate);
    assert!(validate_legacy_history_blocks(&encoded, &rows, &duplicate_native).is_err());
    let mut changed = rows.clone(); changed[512]["text"] = json!("changed");
    assert!(validate_legacy_history_blocks(&encoded, &changed, &parsed.tiptap_json).is_err());
    let duplicate_xml = "<paragraph data-block-id=\"history-index-512\">Row 512: <literal> & exact text</paragraph>";
    assert!(legacy_history_export_xml(&(xml + duplicate_xml), &rows).is_err());
}

#[test]
fn mapped_history_custody_preserves_original_records_and_exact_recovery_join() {
    let record = json!({"snapshot_id":{"S":"s"},"doc_id":{"S":"old.name"},
        "doc_key":{"S":"g#old.name"},"owner_user_id":{"S":"owner"},"graph_id":{"S":"g"}});
    let source_bytes = json_bytes(&record).unwrap();
    let manifest = json!({"transformation":V23,"sourceRecords":[{"record_json":String::from_utf8(source_bytes.clone()).unwrap()}],
        "compatibilityTransform":{"rule":"vera-20260920-standard-compatibility-v1",
            "originalCustodyUnchanged":true,"identifierMap":{"old.name":"new-name"}}});
    let recovery = json!({"snapshotId":"s","documentId":"old.name","recordSha256":archive_sha256(&source_bytes),
        "bucket":"mnemosyne-dev-prod-crdt-state","key":"users/owner/graphs/g/document-snapshots/old.name/s.json",
        "disposition":"semantic-history-present"});
    let custody = json!({"userId":"owner","graphId":"g","dynamoRecords":[{"table":"mnemosyne-document-snapshots","record":record}],
        "documentHistoryRecovery":{"mode":"available-payloads-only-v1","unavailablePayloads":"not-substituted-not-fabricated",
            "snapshots":[recovery]}});
    let mut members = Members::new();
    members.insert("source-custody/index.json".into(), json_bytes(&custody).unwrap());
    members.insert("history/documents/index.json".into(), json_bytes(&json!([
        {"snapshot_id":"s","doc_id":"new-name","member":"history/documents/s.json"}])).unwrap());
    members.insert("history/documents/s.json".into(), b"source-body".to_vec());
    let before = members.clone();
    let (missing, availability) = prepare_history_availability(&members,&manifest,"g","owner").unwrap();
    assert!(missing.is_empty());
    assert_eq!(availability["availablePayloadCount"], 1);
    assert_eq!(availability["entries"][0]["documentId"], "old.name");
    assert_eq!(availability["entries"][0]["recordSha256"], archive_sha256(&source_bytes));
    assert_eq!(members, before);
    for mutation in ["rule", "custody", "missing", "unsafe", "self", "metadata"] {
        let mut bad = manifest.clone();
        match mutation {
            "rule" => bad["compatibilityTransform"]["rule"] = json!("unknown"),
            "custody" => bad["compatibilityTransform"]["originalCustodyUnchanged"] = json!(false),
            "missing" => bad["compatibilityTransform"]["identifierMap"] = json!({}),
            "unsafe" => bad["compatibilityTransform"]["identifierMap"]["old.name"] = json!("../wrong"),
            "self" => bad["compatibilityTransform"]["identifierMap"]["old.name"] = json!("old.name"),
            "metadata" => { let mut changed=record.clone();changed["graph_id"]=json!({"S":"foreign"});
                bad["sourceRecords"][0]["record_json"]=json!(String::from_utf8(json_bytes(&changed).unwrap()).unwrap()); },
            _ => unreachable!(),
        }
        assert!(prepare_history_availability(&members,&bad,"g","owner").is_err(),"{mutation}");
    }
    let mut changed=custody.clone();changed["documentHistoryRecovery"]["snapshots"][0]["documentId"]=json!("new-name");
    members.insert("source-custody/index.json".into(),json_bytes(&changed).unwrap());
    assert!(prepare_history_availability(&members,&manifest,"g","owner").is_err());
}
