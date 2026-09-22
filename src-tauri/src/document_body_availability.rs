//! Imported absence is not an empty document and must never enter ghost healing.
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::Path;

pub(crate) const RELATIVE_PATH: &str = ".migration/preservation-v2/unavailable-bodies.json";

pub(crate) fn unavailable_documents(graph_dir: &Path) -> Result<Vec<Value>, String> {
    let path = graph_dir.join(RELATIVE_PATH);
    match std::fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let claim_path = graph_dir.join(".migration/preservation-v2/claim.json");
            if claim_path.exists() {
                let claim: Value = crate::storage::read_json(&claim_path)?;
                if claim.get("unavailableBodiesSha256").is_some() {
                    return Err("source body availability marker missing".into());
                }
            }
            return Ok(Vec::new());
        }
        Err(e) => return Err(e.to_string()),
        Ok(meta) if !meta.is_file() || meta.file_type().is_symlink() => {
            return Err("invalid source body availability file".into())
        }
        Ok(_) => {}
    }
    let index: Value = crate::storage::read_json(&path)?;
    let claim: Value =
        crate::storage::read_json(&graph_dir.join(".migration/preservation-v2/claim.json"))?;
    if index["schemaVersion"] != 1
        || index["state"] != "source-body-unavailable"
        || [
            "operationId",
            "archiveSha256",
            "sourceUserId",
            "sourceGraphId",
            "targetGraphIncarnation",
            "targetGeneration",
        ]
        .iter()
        .any(|key| index[*key].is_null() || index[*key] != claim[*key])
        || index["sourceGraphId"].as_str() != graph_dir.file_name().and_then(|n| n.to_str())
    {
        return Err("source body availability claim mismatch".into());
    }
    let rows = index["documents"]
        .as_array()
        .ok_or("invalid unavailable document inventory")?;
    let rows_hash = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(rows).map_err(|e| e.to_string())?)
    );
    if claim["unavailableBodiesSha256"] != rows_hash {
        return Err("source body availability inventory hash mismatch".into());
    }
    let mut ids = std::collections::BTreeSet::new();
    for row in rows {
        let id = row["documentId"]
            .as_str()
            .ok_or("missing unavailable document ID")?;
        crate::ids::validate_local_id(id, "unavailable document ID")?;
        if !ids.insert(id)
            || row["reason"] != "absent-current-saved-object"
            || row["sourceUserId"] != claim["sourceUserId"]
            || row["sourceGraphId"] != claim["sourceGraphId"]
            || !row["inventorySha256"].as_str().is_some_and(|hash| {
                hash.len() == 64
                    && hash
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            })
            || row["metadata"].as_object().is_none()
        {
            return Err("invalid unavailable document identity/metadata".into());
        }
    }
    Ok(rows.clone())
}

pub(crate) fn require_available(graph_dir: &Path, document_id: &str) -> Result<(), String> {
    if unavailable_documents(graph_dir)?
        .iter()
        .any(|row| row["documentId"] == document_id)
    {
        return Err(format!(
            "source_body_unavailable: {document_id}; retained source metadata is not an empty body"
        ));
    }
    Ok(())
}

pub(crate) fn require_state_path_available(path: &Path) -> Result<(), String> {
    if let Some((graph_dir, id)) =
        crate::document_tombstone_store::document_identity_from_ydoc_state_path(path)?
    {
        require_available(&graph_dir, &id)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{fs, path::PathBuf};

    fn fixture() -> PathBuf {
        let root = std::env::temp_dir()
            .join(format!("unavailable-body-{}", uuid::Uuid::new_v4()))
            .join("graphs")
            .join("fixture-graph");
        fs::create_dir_all(root.join(".migration/preservation-v2")).unwrap();
        let mut claim = json!({"schemaVersion":1,"operationId":"fixture-import", "archiveSha256":"a".repeat(64),
            "sourceUserId":"fixture-owner","sourceGraphId":"fixture-graph", "targetGraphIncarnation":"fixture-incarnation","targetGeneration":1});
        let rows = json!([{"documentId":"missing-slot","reason":"absent-current-saved-object",
            "sourceUserId":"fixture-owner","sourceGraphId":"fixture-graph","inventorySha256":"b".repeat(64),
            "metadata":{"title":"Unavailable source","readOnly":false}}]);
        claim["unavailableBodiesSha256"] = json!(format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&rows).unwrap())
        ));
        fs::write(
            root.join(".migration/preservation-v2/claim.json"),
            serde_json::to_vec(&claim).unwrap(),
        )
        .unwrap();
        let mut index = claim;
        index["state"] = json!("source-body-unavailable");
        index["documents"] = rows;
        fs::write(
            root.join(RELATIVE_PATH),
            serde_json::to_vec(&index).unwrap(),
        )
        .unwrap();
        root
    }

    #[test]
    fn unavailable_body_refuses_empty_sidecar_and_preserves_real_empty_body() {
        let root = fixture();
        let missing = crate::paths::document_ydoc_state_path(&root, "missing-slot");
        let error = crate::document_sidecar_store::write_ydoc_update(&missing, "").unwrap_err();
        assert!(error.starts_with("source_body_unavailable:"));
        assert!(!missing.parent().unwrap().exists());
        let present = crate::paths::document_ydoc_state_path(&root, "present-empty");
        crate::document_sidecar_store::write_ydoc_update(&present, "").unwrap();
        assert!(!fs::read(present).unwrap().is_empty());
        assert_eq!(unavailable_documents(&root).unwrap().len(), 1);
    }

    #[test]
    fn unavailable_body_refuses_foreign_claim_and_malformed_marker() {
        for mutation in [
            "sourceGraphId",
            "sourceUserId",
            "archiveSha256",
            "operationId",
            "documents",
        ] {
            let root = fixture();
            let path = root.join(RELATIVE_PATH);
            let mut index: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            index[mutation] = json!("foreign");
            fs::write(&path, serde_json::to_vec(&index).unwrap()).unwrap();
            assert!(require_available(&root, "missing-slot").is_err());
        }
    }

    #[test]
    fn unavailable_body_is_not_a_tombstone_or_recreation_capability() {
        let root = fixture();
        assert!(
            !crate::document_tombstone_store::document_is_tombstoned(&root, "missing-slot")
                .unwrap()
        );
        let tombstone = crate::document_tombstone_store::write_document_tombstone_for_operation(
            &root,
            "missing-slot",
            Some("old-delete"),
        )
        .unwrap();
        assert!(
            crate::document_tombstone_store::require_document_tombstone_matches(
                &root,
                "missing-slot",
                &tombstone.deletion_id
            )
            .unwrap_err()
            .starts_with("source_body_unavailable:")
        );
        assert!(root.join(RELATIVE_PATH).is_file());
    }

    #[test]
    fn unavailable_body_sync_room_never_materializes_empty_state() {
        let root = fixture();
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let registry = crate::crdt_engine::rooms::RoomRegistry::default();
            let error = crate::loopback_hocuspocus_routes::document_exists_for_room_connection(
                &registry, "fixture-graph", &root, "missing-slot").await.unwrap_err();
            assert!(error.starts_with("source_body_unavailable:"));
            let path = crate::paths::document_ydoc_state_path(&root, "missing-slot");
            assert!(registry
                .get_or_create("doc:fixture-graph:missing-slot", path.clone())
                .await
                .err()
                .unwrap()
                .starts_with("source_body_unavailable:"));
            assert!(!path.parent().unwrap().exists());
        });
    }

    #[test]
    fn unavailable_body_batch_refuses_before_workspace_mutation() {
        use yrs::{Doc, Transact, ReadTxn, StateVector};
        let root = fixture();
        for missing_first in [false, true] {
            let doc = Doc::new();
            let before = doc.transact().encode_state_as_update_v1(&StateVector::default());
            let mut rows = vec![json!({"documentId":"present-empty"}), json!({"documentId":"missing-slot"})];
            if missing_first { rows.reverse(); }
            let result = crate::crdt_engine::workspace_ops::batch_register_in_doc(
                &mut doc.transact_mut(), &root, &json!({"batchId":"batch","documents":rows}), "1789150000000");
            assert!(result.unwrap_err().starts_with("source_body_unavailable:"));
            assert_eq!(before, doc.transact().encode_state_as_update_v1(&StateVector::default()));
        }
    }

    #[test]
    fn unavailable_body_folder_cascade_refuses_before_any_removal() {
        use yrs::{Doc, Transact, ReadTxn, StateVector, Map, MapPrelim};
        let root = fixture();
        let doc = Doc::new();
        let folders=doc.get_or_insert_map("folders");
        let documents=doc.get_or_insert_map("documents");
        let _artifacts=doc.get_or_insert_map("artifacts");
        {
            let mut txn=doc.transact_mut();
            let folder=folders.insert(&mut txn,"folder",MapPrelim::default());
            folder.insert(&mut txn,"name","Folder");
            for id in ["present-empty","missing-slot"] {
                let row=documents.insert(&mut txn,id,MapPrelim::default());
                row.insert(&mut txn,"parentId","folder");
            }
        }
        let before=doc.transact().encode_state_as_update_v1(&StateVector::default());
        let result=crate::crdt_engine::workspace_ops::delete_workspace_folder_in_doc_checked(
            &mut doc.transact_mut(),"folder",true,Some(&root));
        assert!(result.unwrap_err().starts_with("source_body_unavailable:"));
        assert_eq!(before,doc.transact().encode_state_as_update_v1(&StateVector::default()));
        assert!(!crate::document_tombstone_store::document_is_tombstoned(&root,"missing-slot").unwrap());
    }

    #[cfg(feature = "headless")]
    #[test]
    fn unavailable_body_normal_listing_read_self_heal_and_create_boundaries() {
        let _serial = crate::tauri_runtime::profile_env_serial()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let root = fixture();
        let previous = std::env::var_os("GARDEN_PROFILE_DIR");
        std::env::set_var(
            "GARDEN_PROFILE_DIR",
            root.parent().unwrap().parent().unwrap(),
        );
        let outcome = std::panic::catch_unwind(|| {
            let record = json!({"graphId":"fixture-graph","title":"Fixture","origin":"local","providerId":"local",
                "localPath":root.to_string_lossy(),"createdAt":"1789150000000","updatedAt":"1789150000000","capabilities":[]});
            fs::write(
                root.join("graph.json"),
                serde_json::to_vec(&record).unwrap(),
            )
            .unwrap();
            let workspace = crate::paths::workspace_snapshot_path(&root);
            fs::create_dir_all(workspace.parent().unwrap()).unwrap();
            fs::write(&workspace, serde_json::to_vec(&json!({"documents":[{"id":"missing-slot","title":"Unavailable source"}],"folders":[],"artifacts":[],"wires":[]})).unwrap()).unwrap();
            let app = crate::tauri_runtime::build_mock_app_for_tests(true);
            let listings =
                crate::document_hosted_projection::hosted_document_summaries(&app, "fixture-graph")
                    .unwrap();
            let listing = serde_json::to_value(listings).unwrap();
            assert_eq!(
                listing[0]["bodyAvailability"]["state"],
                "source-body-unavailable"
            );
            assert_eq!(
                listing[0]["bodyAvailability"]["sourceUserId"],
                "fixture-owner"
            );
            assert_eq!(
                listing[0]["bodyAvailability"]["inventorySha256"],
                "b".repeat(64)
            );
            assert!(listing[0]["revision"].is_null());
            let mcp = crate::document_mcp_service::mcp_local_list_documents(
                app.clone(),
                &json!({"graphId":"fixture-graph"}),
            )
            .unwrap();
            assert_eq!(mcp["documents"][0]["documentId"], listing[0]["id"]);
            assert_eq!(
                mcp["documents"][0]["bodyAvailability"]["state"],
                "source-body-unavailable"
            );
            assert!(mcp["documents"][0]["revision"].is_null());
            assert!(mcp["documents"][0]["blockCount"].is_null());
            assert_eq!(
                unavailable_documents(&root).unwrap()[0]["metadata"]["readOnly"],
                false
            );
            assert_eq!(listing[0]["readOnly"], true);
            assert!(crate::document_delete_service::delete_document_for_operation(
                app.clone(), "fixture-graph".into(), "missing-slot".into(), Some("refused-delete"))
                .unwrap_err().starts_with("source_body_unavailable:"));
            assert!(!crate::document_tombstone_store::document_is_tombstoned(&root, "missing-slot").unwrap());
            for cold in [false, true] {
                let result = if cold {
                    crate::document_service::read_document_cold(
                        app.clone(),
                        "fixture-graph".into(),
                        "missing-slot".into(),
                    )
                } else {
                    crate::document_service::read_document(
                        app.clone(),
                        "fixture-graph".into(),
                        "missing-slot".into(),
                    )
                };
                assert!(result.unwrap_err().starts_with("source_body_unavailable:"));
            }
            assert!(
                crate::document_paths::self_heal_missing_document_with_lease(
                    &app,
                    &root,
                    "fixture-graph",
                    "missing-slot"
                )
                .unwrap_err()
                .starts_with("source_body_unavailable:")
            );
            assert!(crate::document_service::create_document_with_lease(
                app,
                crate::document_types::CreateDocumentInput {
                    graph_id: "fixture-graph".into(),
                    document_id: Some("missing-slot".into()),
                    title: "Stale create".into()
                }
            )
            .unwrap_err()
            .starts_with("source_body_unavailable:"));
            assert!(!root.join("documents/missing-slot/document.json").exists());
            assert!(!crate::paths::document_ydoc_state_path(&root, "missing-slot").exists());
        });
        match previous {
            Some(value) => std::env::set_var("GARDEN_PROFILE_DIR", value),
            None => std::env::remove_var("GARDEN_PROFILE_DIR"),
        };
        if let Err(e) = outcome {
            std::panic::resume_unwind(e)
        }
    }

    #[test]
    fn unavailable_body_fault_is_structured_for_rest_and_mcp() {
        let error = crate::app_error::AppError::internal("source_body_unavailable: fixture");
        assert_eq!(error.kind(), crate::app_error::AppErrorKind::Conflict);
        assert_eq!(
            error.code(),
            Some(crate::app_error_codes::SOURCE_BODY_UNAVAILABLE)
        );
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let response = crate::loopback_http::loopback_error(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                error.message_ref(),
            );
            assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
            let bytes = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            let body: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(body["code"], "source_body_unavailable");
        });
    }
}
