//! Inert archive boundary. No provider, runtime event, graph write or network effect.
//! The caller supplies independently established capture/read scope; payload
//! owner/audience labels are never authority. Persistence and HTTP integration
//! are deliberately separate from this first reader/ontology compatibility unit.
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

pub const SCHEMA: &str = include_str!("emporium/vocabs/sophia-conversation-archive.schema.json");
pub const VOCAB: &str = include_str!("emporium/vocabs/sophia-conversation.golden.json");
type Checked<T> = Result<T, &'static str>;

pub struct ArchiveScope<'a> {
    pub owner: &'a str,
    pub graph: &'a str,
    pub namespace: &'a str,
    pub instance: &'a str,
    /// Exact independently retained binding source reference, not copied out
    /// of an untrusted archive's audience field by the admission caller.
    pub audience_evidence: &'a Value,
    pub artifact_digests: &'a BTreeSet<String>,
    pub readers: &'a BTreeSet<String>,
    /// Attributed references whose source identity mapping was independently
    /// established. A display name/role is not a principal or an Agent grant.
    pub author_refs: &'a BTreeSet<String>,
    pub parent_source: Option<&'a Value>,
}

pub struct ValidatedArchive {
    pub id: String,
    pub bytes_sha256: String,
    pub message_count: usize,
    pub part_count: usize,
    value: Value,
}

fn require(ok: bool, code: &'static str) -> Checked<()> {
    if ok {
        Ok(())
    } else {
        Err(code)
    }
}
fn text(v: &Value) -> Checked<&str> {
    let s = v.as_str().ok_or("archive_string_required")?;
    require(!s.is_empty() && !s.contains('\0'), "archive_empty_or_nul")?;
    Ok(s)
}
fn nullable_text(v: &Value) -> Checked<()> {
    require(
        v.is_null() || v.is_string(),
        "archive_nullable_string_required",
    )
}
fn object(v: &Value, keys: &[&str]) -> Checked<()> {
    let o = v.as_object().ok_or("archive_object_required")?;
    require(
        o.len() == keys.len() && keys.iter().all(|k| o.contains_key(*k)),
        "archive_fields_mismatch",
    )
}
fn array(v: &Value) -> Checked<&Vec<Value>> {
    v.as_array().ok_or("archive_array_required")
}
fn digest(v: &str) -> bool {
    v.len() == 64
        && v.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn principal(v: &str) -> bool {
    let Some((kind, id)) = v.split_once(':') else {
        return false;
    };
    matches!(kind, "user" | "agent" | "service" | "organization")
        && (1..=192).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.@:-".contains(&b))
}
fn source_ref(v: &Value, scope: &ArchiveScope<'_>) -> Checked<()> {
    object(
        v,
        &[
            "artifactSha256",
            "artifactLocator",
            "recordSha256",
            "recordLocator",
        ],
    )?;
    let artifact = text(&v["artifactSha256"])?;
    require(
        digest(artifact) && digest(text(&v["recordSha256"])?),
        "archive_digest_invalid",
    )?;
    require(
        scope.artifact_digests.contains(artifact),
        "archive_unbound_artifact",
    )?;
    text(&v["artifactLocator"])?;
    text(&v["recordLocator"])?;
    Ok(())
}

pub fn entity_id(source: &Value, kind: &str) -> Checked<String> {
    object(
        source,
        &["namespace", "instance", "identityBasis", "nativeId"],
    )?;
    require(
        matches!(kind, "conversation" | "message" | "part"),
        "archive_kind_invalid",
    )?;
    let basis = text(&source["identityBasis"])?;
    require(
        matches!(basis, "native-id" | "record-locator"),
        "archive_identity_basis_invalid",
    )?;
    let fields = [
        "sophia.conversation-id.v1",
        text(&source["namespace"])?,
        text(&source["instance"])?,
        kind,
        basis,
        text(&source["nativeId"])?,
    ];
    let encoded = serde_json::to_vec(&fields).map_err(|_| "archive_identity_encoding")?;
    Ok(format!(
        "urn:sophia:conversation-archive:{kind}:{:x}",
        Sha256::digest(encoded)
    ))
}
fn bound_id(source: &Value, kind: &str, actual: &Value, scope: &ArchiveScope<'_>) -> Checked<()> {
    require(
        text(&source["namespace"])? == scope.namespace
            && text(&source["instance"])? == scope.instance,
        "archive_source_scope_mismatch",
    )?;
    require(
        entity_id(source, kind)? == text(actual)?,
        "archive_identity_mismatch",
    )
}
fn reference(v: &Value, kind: &str) -> Checked<()> {
    if v.is_null() {
        return Ok(());
    }
    let prefix = format!("urn:sophia:conversation-archive:{kind}:");
    require(
        text(v)?.strip_prefix(&prefix).is_some_and(digest),
        "archive_reference_invalid",
    )
}

/// Bounded bytes are an admission limit, not a claim that partial archives are
/// complete. Large sources must use a separately qualified paged representation.
pub fn validate(bytes: &[u8], scope: &ArchiveScope<'_>) -> Checked<ValidatedArchive> {
    require(bytes.len() <= 8 * 1024 * 1024, "archive_envelope_capacity")?;
    require(
        principal(scope.owner) && !scope.graph.is_empty(),
        "archive_scope_invalid",
    )?;
    require(
        scope.readers.len() == 1 && scope.readers.contains(scope.owner),
        "archive_reader_audience_hold",
    )?;
    let v: Value = serde_json::from_slice(bytes).map_err(|_| "archive_json_invalid")?;
    object(
        &v,
        &[
            "schema",
            "conversationId",
            "parentConversationId",
            "ownerPrincipal",
            "graphId",
            "audience",
            "source",
            "revision",
            "title",
            "messages",
            "completeness",
        ],
    )?;
    require(
        v["schema"] == "sophia.conversation-archive.v1",
        "archive_schema_invalid",
    )?;
    require(
        v["ownerPrincipal"] == scope.owner && v["graphId"] == scope.graph,
        "archive_owner_graph_mismatch",
    )?;
    object(&v["audience"], &["kind", "ownerPrincipal", "evidence"])?;
    require(
        v["audience"]["kind"] == "owner-only" && v["audience"]["ownerPrincipal"] == scope.owner,
        "archive_audience_hold",
    )?;
    let evidence = array(&v["audience"]["evidence"])?;
    require(
        !evidence.is_empty() && evidence.contains(scope.audience_evidence),
        "archive_audience_unbound",
    )?;
    for r in evidence {
        source_ref(r, scope)?;
    }
    bound_id(&v["source"], "conversation", &v["conversationId"], scope)?;
    reference(&v["parentConversationId"], "conversation")?;
    require(
        v["parentConversationId"] != v["conversationId"],
        "archive_self_parent",
    )?;
    if !v["parentConversationId"].is_null() {
        bound_id(
            scope.parent_source.ok_or("archive_parent_unbound")?,
            "conversation",
            &v["parentConversationId"],
            scope,
        )?;
    }
    object(&v["revision"], &["sourceRecords"])?;
    let revisions = array(&v["revision"]["sourceRecords"])?;
    require(!revisions.is_empty(), "archive_revision_missing")?;
    for r in revisions {
        source_ref(r, scope)?;
    }
    nullable_text(&v["title"])?;
    object(&v["completeness"], &["status", "reason"])?;
    require(
        matches!(
            text(&v["completeness"]["status"])?,
            "complete" | "partial" | "unknown"
        ),
        "archive_completeness_invalid",
    )?;
    nullable_text(&v["completeness"]["reason"])?;
    if v["completeness"]["status"] != "complete" {
        text(&v["completeness"]["reason"])?;
    }
    let messages = array(&v["messages"])?;
    let mut message_ids = BTreeSet::new();
    let mut part_ids = BTreeSet::new();
    for m in messages {
        object(
            m,
            &[
                "messageId",
                "source",
                "sourceRef",
                "state",
                "deletionSourceRef",
                "role",
                "authorRef",
                "occurredAt",
                "replyTo",
                "parts",
            ],
        )?;
        bound_id(&m["source"], "message", &m["messageId"], scope)?;
        require(
            message_ids.insert(text(&m["messageId"])?),
            "archive_duplicate_message",
        )?;
        source_ref(&m["sourceRef"], scope)?;
        match text(&m["state"])? {
            "present" => require(m["deletionSourceRef"].is_null(), "archive_false_deletion")?,
            "deleted" => source_ref(&m["deletionSourceRef"], scope)?,
            _ => return Err("archive_message_state_invalid"),
        }
        text(&m["role"])?;
        nullable_text(&m["authorRef"])?;
        nullable_text(&m["occurredAt"])?;
        if let Some(author) = m["authorRef"].as_str() {
            require(scope.author_refs.contains(author), "archive_author_unbound")?;
        }
        reference(&m["replyTo"], "message")?;
        require(m["replyTo"] != m["messageId"], "archive_self_reply")?;
        for p in array(&m["parts"])? {
            object(
                p,
                &[
                    "partId",
                    "source",
                    "kind",
                    "mediaType",
                    "content",
                    "sourceRef",
                    "redactions",
                ],
            )?;
            bound_id(&p["source"], "part", &p["partId"], scope)?;
            require(
                part_ids.insert(text(&p["partId"])?),
                "archive_duplicate_part",
            )?;
            require(
                matches!(
                    text(&p["kind"])?,
                    "text"
                        | "reasoning"
                        | "tool-activity"
                        | "tool-call"
                        | "tool-result"
                        | "attachment"
                        | "unknown"
                ),
                "archive_part_kind_invalid",
            )?;
            text(&p["mediaType"])?;
            require(p["content"].is_string(), "archive_content_string_required")?;
            match text(&p["kind"])? {
                "text" | "reasoning" => require(
                    p["mediaType"] == "text/plain",
                    "archive_text_profile_invalid",
                )?,
                "tool-activity" => {
                    require(
                        p["mediaType"] == "application/vnd.sophia.tool-activity+json",
                        "archive_tool_profile_invalid",
                    )?;
                    let t: Value = serde_json::from_str(p["content"].as_str().unwrap())
                        .map_err(|_| "archive_tool_json_invalid")?;
                    object(
                        &t,
                        &[
                            "toolName",
                            "callId",
                            "state",
                            "input",
                            "output",
                            "error",
                            "extensions",
                        ],
                    )?;
                    nullable_text(&t["toolName"])?;
                    nullable_text(&t["callId"])?;
                    nullable_text(&t["state"])?;
                    require(
                        t["extensions"].is_object(),
                        "archive_tool_extensions_invalid",
                    )?;
                }
                _ => (),
            }
            source_ref(&p["sourceRef"], scope)?;
            for r in array(&p["redactions"])? {
                object(r, &["reason", "sourceLocator"])?;
                text(&r["reason"])?;
                text(&r["sourceLocator"])?;
            }
        }
    }
    // This first profile holds unresolved reply topology without altering raw
    // custody; a partial export is not permission to invent a target's domain.
    for m in messages {
        if let Some(id) = m["replyTo"].as_str() {
            require(message_ids.contains(id), "archive_unresolved_reply")?;
        }
    }
    Ok(ValidatedArchive {
        id: text(&v["conversationId"])?.to_owned(),
        bytes_sha256: format!("{:x}", Sha256::digest(bytes)),
        message_count: messages.len(),
        part_count: part_ids.len(),
        value: v,
    })
}

fn metadata_records(
    a: &ValidatedArchive,
) -> Checked<Vec<crate::emporium::schemas::GenericRecordIn>> {
    use serde_json::json;
    use std::collections::BTreeMap;
    let v = &a.value;
    let mut sources = BTreeMap::<String, Value>::new();
    let mut source_record = |r: &Value| -> Checked<String> {
        let key = serde_json::to_vec(&[
            text(&r["artifactSha256"])?,
            text(&r["recordSha256"])?,
            text(&r["recordLocator"])?,
        ])
        .map_err(|_| "archive_projection_source")?;
        let id = format!(
            "urn:sophia:conversation-source-record:{:x}",
            Sha256::digest(key)
        );
        sources.insert(id.clone(),json!({"kind":"SourceRecord","localId":id,"artifactDigest":r["artifactSha256"],"recordDigest":r["recordSha256"]}));
        Ok(id)
    };
    let mut c = json!({"kind":"Conversation","localId":a.id,"sourceInstance":v["source"]["instance"],"sourceNativeId":v["source"]["nativeId"],"revisionDigest":a.bytes_sha256});
    if !v["parentConversationId"].is_null() {
        c["parentConversation"] = v["parentConversationId"].clone();
    }
    let mut rows = vec![c];
    for (ordinal, m) in array(&v["messages"])?.iter().enumerate() {
        let mut row = json!({"kind":"Message","localId":m["messageId"],"inConversation":a.id,"ordinal":ordinal,"sourceRecord":source_record(&m["sourceRef"])?});
        if !m["replyTo"].is_null() {
            row["replyTo"] = m["replyTo"].clone();
        }
        if !m["authorRef"].is_null() {
            row["authorRef"] = m["authorRef"].clone();
        }
        rows.push(row);
        for (ordinal, p) in array(&m["parts"])?.iter().enumerate() {
            rows.push(json!({"kind":"Part","localId":p["partId"],"inMessage":m["messageId"],"ordinal":ordinal,"partKind":p["kind"],"sourceRecord":source_record(&p["sourceRef"])?}));
        }
    }
    rows.extend(sources.into_values());
    rows.into_iter()
        .map(|v| serde_json::from_value(v).map_err(|_| "archive_projection_record_invalid"))
        .collect()
}

/// Exercises the existing Emporium schema, class dispatch, generic planner and
/// SHACL validation entirely in memory. Not registered, persisted or published.
/// Even the derived metadata is private until a later real audience-aware read.
pub fn validate_projection(a: &ValidatedArchive) -> Checked<usize> {
    let c: crate::emporium::contract::VocabularyContract =
        serde_json::from_str(VOCAB).map_err(|_| "archive_vocab_invalid")?;
    let rows = metadata_records(a)?;
    crate::emporium::schemas::IngestRequest::validate_generic(&c, &rows)
        .map_err(|_| "archive_projection_schema_invalid")?;
    let plan =
        crate::emporium::planner::plan_generic_compute(&c, text(&a.value["graphId"])?, &rows)
            .map_err(|_| "archive_projection_plan_invalid")?;
    crate::emporium::shacl_validator::validate_desired(&plan.desired_inserts, &c)
        .map_err(|_| "archive_projection_shape_invalid")?;
    Ok(plan.desired_inserts.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn source(id: &str) -> Value {
        json!({"namespace":"zulip","instance":"https://realm-a.example","identityBasis":"native-id","nativeId":id})
    }
    fn witness() -> Value {
        json!({"artifactSha256":"a".repeat(64),"artifactLocator":"synthetic-source.json","recordSha256":"b".repeat(64),"recordLocator":"/messages/0"})
    }
    fn fixture() -> Value {
        let c = source("thread-1");
        let m = source("message-1");
        let p = source("message-1/part-0");
        json!({"schema":"sophia.conversation-archive.v1","conversationId":entity_id(&c,"conversation").unwrap(),"parentConversationId":null,
            "ownerPrincipal":"user:fixture","graphId":"synthetic","audience":{"kind":"owner-only","ownerPrincipal":"user:fixture","evidence":[witness()]},
            "source":c,"revision":{"sourceRecords":[witness()]},"title":"Synthetic history","completeness":{"status":"complete","reason":null},
            "messages":[{"messageId":entity_id(&m,"message").unwrap(),"source":m,"sourceRef":witness(),"state":"present","deletionSourceRef":null,
                "role":"assistant","authorRef":null,"occurredAt":null,"replyTo":null,"parts":[{"partId":entity_id(&p,"part").unwrap(),"source":p,
                    "kind":"tool-call","mediaType":"application/json","content":"{\"name\":\"do-not-execute\",\"state\":\"recorded\"}","sourceRef":witness(),"redactions":[]}]}]})
    }
    fn check(
        v: &Value,
        owner: &str,
        graph: &str,
        readers: BTreeSet<String>,
    ) -> Checked<ValidatedArchive> {
        let evidence = witness();
        let artifacts = BTreeSet::from(["a".repeat(64)]);
        let authors = BTreeSet::new();
        validate(
            &serde_json::to_vec(v).unwrap(),
            &ArchiveScope {
                owner,
                graph,
                namespace: "zulip",
                instance: "https://realm-a.example",
                audience_evidence: &evidence,
                artifact_digests: &artifacts,
                readers: &readers,
                author_refs: &authors,
                parent_source: None,
            },
        )
    }
    fn normal(v: &Value) -> Checked<ValidatedArchive> {
        check(
            v,
            "user:fixture",
            "synthetic",
            BTreeSet::from(["user:fixture".into()]),
        )
    }
    #[test]
    fn conversation_archive_inert_payload_and_source_revision_are_distinct() {
        let v = fixture();
        let first = normal(&v).unwrap();
        assert_eq!((first.message_count, first.part_count), (1, 1));
        let mut edited = v.clone();
        edited["messages"][0]["parts"][0]["content"] = json!("an edited historical record");
        edited["revision"]["sourceRecords"][0]["recordSha256"] = json!("c".repeat(64));
        let second = normal(&edited).unwrap();
        assert_eq!(first.id, second.id);
        assert_ne!(first.bytes_sha256, second.bytes_sha256);
        assert_eq!(
            v["messages"][0]["parts"][0]["content"],
            fixture()["messages"][0]["parts"][0]["content"]
        );
    }
    #[test]
    fn conversation_archive_realm_and_identity_basis_prevent_collisions() {
        let a = source("42");
        let mut b = a.clone();
        b["instance"] = json!("https://realm-b.example");
        assert_ne!(
            entity_id(&a, "message").unwrap(),
            entity_id(&b, "message").unwrap()
        );
        b = a.clone();
        b["identityBasis"] = json!("record-locator");
        assert_ne!(
            entity_id(&a, "part").unwrap(),
            entity_id(&b, "part").unwrap()
        );
        assert_ne!(
            entity_id(&a, "message").unwrap(),
            entity_id(&a, "conversation").unwrap()
        );
    }
    #[test]
    fn conversation_archive_owner_graph_and_reader_superset_refuse() {
        let v = fixture();
        let owner = BTreeSet::from(["user:fixture".into()]);
        assert!(check(&v, "user:other", "synthetic", owner.clone()).is_err());
        assert!(check(&v, "user:fixture", "other", owner).is_err());
        assert_eq!(
            check(
                &v,
                "user:fixture",
                "synthetic",
                BTreeSet::from(["user:fixture".into(), "user:reader".into()])
            )
            .err(),
            Some("archive_reader_audience_hold")
        );
    }
    #[test]
    fn conversation_archive_identity_duplicates_and_executable_fields_refuse() {
        let mut v = fixture();
        v["commands"] = json!(["run"]);
        assert!(normal(&v).is_err());
        v = fixture();
        v["schema"] = json!("choreograph.agent-session-event.v1");
        assert!(normal(&v).is_err());
        v = fixture();
        v["conversationId"] =
            json!("urn:sophia:conversation-archive:conversation:".to_owned() + &"0".repeat(64));
        assert_eq!(normal(&v).err(), Some("archive_identity_mismatch"));
        v = fixture();
        let p = v["messages"][0]["parts"][0].clone();
        v["messages"][0]["parts"].as_array_mut().unwrap().push(p);
        assert_eq!(normal(&v).err(), Some("archive_duplicate_part"));
        v = fixture();
        let m = v["messages"][0].clone();
        v["messages"].as_array_mut().unwrap().push(m);
        assert_eq!(normal(&v).err(), Some("archive_duplicate_message"));
    }
    #[test]
    fn conversation_archive_missing_authority_and_false_deletion_refuse() {
        let mut v = fixture();
        v["audience"]["evidence"] = json!([]);
        assert!(normal(&v).is_err());
        v = fixture();
        v["messages"][0]["state"] = json!("deleted");
        assert!(normal(&v).is_err());
        v["messages"][0]["deletionSourceRef"] = witness();
        assert!(normal(&v).is_ok());
        v["messages"][0]["state"] = json!("unavailable");
        assert_eq!(normal(&v).err(), Some("archive_message_state_invalid"));
        v = fixture();
        v["messages"][0]["authorRef"] = json!("urn:sophia:agent:inferred-from-label");
        assert_eq!(normal(&v).err(), Some("archive_author_unbound"));
    }
    #[test]
    fn conversation_archive_topology_and_foreign_source_refuse() {
        let mut v = fixture();
        v["parentConversationId"] = json!(entity_id(&source("parent"), "conversation").unwrap());
        assert_eq!(normal(&v).err(), Some("archive_parent_unbound"));
        v = fixture();
        v["messages"][0]["replyTo"] = json!(entity_id(&source("unobserved"), "message").unwrap());
        assert_eq!(normal(&v).err(), Some("archive_unresolved_reply"));
        v = fixture();
        v["messages"][0]["source"]["instance"] = json!("https://other.example");
        assert_eq!(normal(&v).err(), Some("archive_source_scope_mismatch"));
    }
    #[test]
    fn conversation_archive_common_text_and_tool_activity_profiles() {
        let mut v = fixture();
        let p = &mut v["messages"][0]["parts"][0];
        p["kind"] = json!("text");
        p["mediaType"] = json!("text/plain");
        p["content"] = json!("Source-neutral text");
        assert!(normal(&v).is_ok());
        v["messages"][0]["parts"][0]["mediaType"] = json!("application/vnd.opencode.part+json");
        assert_eq!(normal(&v).err(), Some("archive_text_profile_invalid"));
        let p = &mut v["messages"][0]["parts"][0];
        p["kind"] = json!("tool-activity");
        p["mediaType"] = json!("application/vnd.sophia.tool-activity+json");
        p["content"]=json!(json!({"toolName":"read","callId":"c1","state":"completed","input":{"path":"synthetic"},"output":"retained","error":null,"extensions":{"nativeUnknown":true}}).to_string());
        assert!(normal(&v).is_ok());
        v["messages"][0]["parts"][0]["content"] = json!("{\"run\":true}");
        assert_eq!(normal(&v).err(), Some("archive_fields_mismatch"));
    }
    #[test]
    fn conversation_archive_vocabulary_uses_existing_emporium_shape_without_publication() {
        let c: crate::emporium::contract::VocabularyContract = serde_json::from_str(VOCAB).unwrap();
        assert_eq!(c.name, "sophia-conversation");
        assert_eq!(c.classes.len(), 4);
        for name in ["Conversation", "Message", "Part", "SourceRecord"] {
            assert!(c.materialization_signature(name).is_ok());
        }
        let shacl = crate::emporium::shacl_emit::vocab_to_shacl(&c);
        assert!(shacl.contains("http://mnemosyne.dev/conversation#Message"));
        assert!(!shacl.contains("sh:targetClass <http://mnemosyne.dev/agent#Turn>"));
        assert!(!crate::emporium::vocabs::VOCAB_REGISTRY
            .iter()
            .any(|(name, _, _)| *name == "sophia-conversation"));
        assert!(!c.classes["Part"]
            .predicates
            .keys()
            .any(|p| p.ends_with(":content") || p.ends_with(":body")));
        let schema: Value = serde_json::from_str(SCHEMA).unwrap();
        assert_eq!(
            schema["properties"]["schema"]["const"],
            "sophia.conversation-archive.v1"
        );
        let archive = normal(&fixture()).unwrap();
        assert!(validate_projection(&archive).unwrap() > 10);
        let mut rows = metadata_records(&archive).unwrap();
        rows[0]
            .fields
            .insert("body".into(), json!("must not silently publish"));
        assert!(crate::emporium::planner::plan_generic_compute(&c, "synthetic", &rows).is_err());
    }
}
