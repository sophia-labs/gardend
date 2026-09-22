//! Optional-use, graph-shared reports joined to the frozen Emporium Agent identity.
//! These are editable RDF testimony, NOT runtime liveness or signed seat identity.
use crate::{
    app_error::{AppError, AppResult},
    app_runtime::AppHandle,
    crdt_engine::persistence_coordinator::GraphPersistenceCoordinator,
};
use chrono::{DateTime, Datelike, Duration, SecondsFormat, Timelike, Utc};
use oxigraph::{
    model::{GraphNameRef, NamedNode, NamedNodeRef, Term},
    store::Store,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
#[cfg(feature = "desktop")]
use tauri::Manager;

const AGENT: &str = "http://mnemosyne.dev/agent#Agent";
const TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
const LABEL: &str = "http://www.w3.org/2000/01/rdf-schema#label";
const REPORT: &str = "urn:sophia:agent-status:reportJson";
const MAX_REPORTS: usize = 256;
const MAX_AGENTS: usize = 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Input {
    graph_incarnation: String,
    agent_id: String,
    body_id: String,
    activity: String,
    ttl_seconds: i64,
    origin: String,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    run_id: Option<String>,
    #[serde(default)]
    seat_label: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Report {
    schema_version: u8,
    input: Input,
    #[serde(with = "timestamp")]
    reported_at: DateTime<Utc>,
    #[serde(with = "timestamp")]
    expires_at: DateTime<Utc>,
    ingress_principal: Option<String>,
}
// The real pane's Date.parse clock has millisecond precision. Author both
// report and read clocks at that precision, so freshness agrees at expiry.
// Stored values are never silently truncated: finer precision is refused.
fn clock_millis(now: DateTime<Utc>) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(now.timestamp_millis())
        .expect("system timestamp is representable")
}
fn timestamp_text(value: &DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Millis, true)
}
mod timestamp {
    use super::*;
    use serde::{Deserializer, Serializer};
    pub(super) fn serialize<S: Serializer>(
        value: &DateTime<Utc>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        if !(1..=9999).contains(&value.year())
            || value.nanosecond() % 1_000_000 != 0
            || value.nanosecond() >= 1_000_000_000
        {
            return Err(serde::ser::Error::custom(
                "status timestamp requires a calendar-valid millisecond UTC instant",
            ));
        }
        serializer.serialize_str(&timestamp_text(value))
    }
    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<DateTime<Utc>, D::Error> {
        let text = String::deserialize(deserializer)?;
        if text.len() > 64 || text.ends_with("-00:00") {
            return Err(serde::de::Error::custom(
                "status timestamp too long or offset unknown",
            ));
        }
        let value = DateTime::parse_from_rfc3339(&text)
            .map_err(serde::de::Error::custom)?
            .with_timezone(&Utc);
        if !(1..=9999).contains(&value.year())
            || value.nanosecond() % 1_000_000 != 0
            || value.nanosecond() >= 1_000_000_000
        {
            return Err(serde::de::Error::custom(
                "status timestamp requires a calendar-valid millisecond instant",
            ));
        }
        Ok(value)
    }
}
fn agent_id_valid(id: &str) -> bool {
    id.len() == 22
        && id.starts_with("agent-")
        && id.as_bytes()[6..]
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
}
fn validate(input: &Input) -> AppResult<()> {
    if !agent_id_valid(&input.agent_id) {
        return Err(AppError::validation(
            "existing canonical agent-<hex16> identity required",
        ));
    }
    crate::ids::validate_local_id(&input.body_id, "bodyId").map_err(AppError::validation)?;
    if input.graph_incarnation.is_empty()
        || input.graph_incarnation.len() > 128
        || input.body_id.len() > 128
        || input.activity.is_empty()
        || input.activity.len() > 2048
        || !(30..=600).contains(&input.ttl_seconds)
        || !["self-reported", "operator-reported"].contains(&input.origin.as_str())
    {
        return Err(AppError::validation(
            "invalid status bounds, origin or expiry",
        ));
    }
    for id in [&input.session_id, &input.run_id].into_iter().flatten() {
        if id.len() > 128 {
            return Err(AppError::validation("status coordinate too long"));
        }
        crate::ids::validate_local_id(id, "status coordinate").map_err(AppError::validation)?;
    }
    if input
        .seat_label
        .as_ref()
        .is_some_and(|s| s.len() > 128 || s.chars().any(char::is_control))
    {
        return Err(AppError::validation("invalid seat label"));
    }
    Ok(())
}
fn key(report: &Report) -> String {
    let bytes = serde_json::to_vec(&json!([
        report.input.graph_incarnation,
        report.input.agent_id,
        report.input.body_id,
        report.ingress_principal
    ]))
    .expect("JSON values serialize");
    format!(
        "urn:sophia:agent-status:{}",
        crate::artifact_text_service::hash(&bytes)
    )
}
fn report_value(report: &Report) -> Value {
    let mut value = serde_json::to_value(&report.input).expect("input serializes");
    value["schemaVersion"] = json!(1);
    value["reportedAt"] = json!(timestamp_text(&report.reported_at));
    value["expiresAt"] = json!(timestamp_text(&report.expires_at));
    value["ingressPrincipal"] = json!(report.ingress_principal);
    value
}
fn graph_id(args: &Value) -> AppResult<String> {
    if let (Some(a), Some(b)) = (args.get("graph_id"), args.get("graphId")) {
        if a != b {
            return Err(AppError::validation("conflicting graph aliases"));
        }
    }
    crate::mcp_utils::mcp_required_graph_id(args).map_err(AppError::validation)
}
fn agents(store: &Store) -> AppResult<BTreeMap<String, Value>> {
    let mut agents = BTreeMap::new();
    let agent = NamedNode::new_unchecked(AGENT);
    for (index, quad) in store
        .quads_for_pattern(
            None,
            Some(NamedNodeRef::new_unchecked(TYPE)),
            Some(agent.as_ref().into()),
            None,
        )
        .enumerate()
    {
        if index >= MAX_AGENTS {
            return Err(AppError::validation(
                "Agent directory exceeds 1024 typed statements; narrower query required",
            ));
        }
        let quad = quad.map_err(|e| AppError::rdf(e.to_string()))?;
        let Some(id) = quad
            .subject
            .to_string()
            .strip_prefix("<urn:sophia:agent:")
            .and_then(|s| s.strip_suffix('>'))
            .map(str::to_owned)
        else {
            continue;
        };
        if !agent_id_valid(&id) {
            continue;
        }
        let mut label = None;
        if let Some(value) = store
            .quads_for_pattern(
                Some(quad.subject.as_ref()),
                Some(NamedNodeRef::new_unchecked(LABEL)),
                None,
                Some(quad.graph_name.as_ref()),
            )
            .next()
        {
            if let Term::Literal(value) = value.map_err(|e| AppError::rdf(e.to_string()))?.object {
                if value.value().len() > 1024 {
                    return Err(AppError::validation("Agent label exceeds read bound"));
                }
                label = Some(value.value().to_owned());
            }
        }
        agents.entry(id.clone()).or_insert_with(|| json!({"agentId":id,"agentIri":format!("urn:sophia:agent:{id}"),"label":label,"reports":[]}));
    }
    Ok(agents)
}
fn reports(store: &Store, graph: &str) -> AppResult<BTreeMap<String, Report>> {
    let graph = crate::rdf_authority::user_rdf_graph_iri(graph);
    let mut reports = BTreeMap::new();
    for (index, quad) in store
        .quads_for_pattern(
            None,
            Some(NamedNodeRef::new_unchecked(REPORT)),
            None,
            Some(GraphNameRef::NamedNode(NamedNodeRef::new_unchecked(&graph))),
        )
        .enumerate()
    {
        if index >= MAX_REPORTS {
            return Err(AppError::validation(
                "status capacity exceeds 256; raw data retained",
            ));
        }
        let quad = quad.map_err(|e| AppError::rdf(e.to_string()))?;
        let Term::Literal(value) = quad.object else {
            return Err(AppError::validation("malformed status literal"));
        };
        if value.value().len() > 8192 {
            return Err(AppError::validation("oversized stored status"));
        }
        let report: Report = serde_json::from_str(value.value())
            .map_err(|e| AppError::validation(format!("malformed stored status: {e}")))?;
        validate(&report.input)?;
        if report.schema_version != 1
            || report.expires_at - report.reported_at != Duration::seconds(report.input.ttl_seconds)
            || report
                .ingress_principal
                .as_ref()
                .is_some_and(|s| s.len() > 1024)
            || quad.subject.to_string() != format!("<{}>", key(&report))
        {
            return Err(AppError::validation(
                "unsupported or inconsistent status; raw data retained",
            ));
        }
        if reports.insert(key(&report), report).is_some() {
            return Err(AppError::validation("duplicate status authority"));
        }
    }
    Ok(reports)
}
fn snapshot(store: &Store, graph: &str, incarnation: &str, now: DateTime<Utc>) -> AppResult<Value> {
    let now = clock_millis(now);
    let mut agents = agents(store)?;
    let mut omitted = 0;
    for report in reports(store, graph)?.into_values() {
        if report.input.graph_incarnation != incarnation {
            omitted += 1;
            continue;
        }
        if report.reported_at > now {
            return Err(AppError::validation(
                "future-dated status is unknown, not recent",
            ));
        }
        if let Some(agent) = agents.get_mut(&report.input.agent_id) {
            let mut value = report_value(&report);
            value["freshness"] = json!(if now < report.expires_at {
                "recent"
            } else {
                "expired"
            });
            agent["reports"]
                .as_array_mut()
                .expect("array initialized")
                .push(value);
        }
    }
    Ok(
        json!({"schemaVersion":1,"graphId":graph,"graphIncarnation":incarnation,"asOf":timestamp_text(&now),"agents":agents.into_values().collect::<Vec<_>>(),"authority":"graph-shared-reports","omittedOtherIncarnations":omitted,"identityCaveat":"Editable graph testimony; captured ingress principal does not authenticate supplied agent, seat or body identity. No process-liveness claim."}),
    )
}
fn publish(
    store: &Store,
    graph: &str,
    incarnation: &str,
    mut input: Input,
    principal: Option<String>,
    now: DateTime<Utc>,
) -> AppResult<Value> {
    let now = clock_millis(now);
    validate(&input)?;
    if input.graph_incarnation != incarnation {
        return Err(AppError::conflict("stale graph incarnation"));
    }
    if principal.as_ref().is_some_and(|p| p.len() > 1024) {
        return Err(AppError::validation("ingress principal too long"));
    }
    if !agents(store)?.contains_key(&input.agent_id) {
        return Err(AppError::validation(
            "Agent is absent from this graph's ontology; status does not create a roster",
        ));
    }
    if principal.is_none() {
        input.origin = "operator-reported".into();
    }
    let expires_at = now + Duration::seconds(input.ttl_seconds);
    let report = Report {
        schema_version: 1,
        input,
        reported_at: now,
        expires_at,
        ingress_principal: principal,
    };
    let key = key(&report);
    let old = reports(store, graph)?;
    if old.len() >= MAX_REPORTS && !old.contains_key(&key) {
        return Err(AppError::conflict(
            "status capacity reached; existing body reports may still be renewed",
        ));
    }
    let target = crate::rdf_authority::user_rdf_graph_iri(graph);
    let encoded =
        serde_json::to_string(&report).map_err(|e| AppError::serialization(e.to_string()))?;
    let literal = crate::rdf::sparql_string_literal(&encoded);
    // One atomic RDF update, serialized with all graph hot writes. Report JSON
    // is the authority; the type/link are readable RDF indexes, not another store.
    let update = format!("DELETE WHERE {{ GRAPH <{target}> {{ <{key}> ?p ?o }} }}; INSERT DATA {{ GRAPH <{target}> {{ <{key}> <{TYPE}> <urn:sophia:agent-status:Report> ; <http://mnemosyne.dev/agent#ofAgent> <urn:sophia:agent:{}> ; <{REPORT}> {literal} . }} }}", report.input.agent_id);
    crate::rdf_query_service::execute_sparql_update(store, &update).map_err(AppError::rdf)?;
    Ok(
        json!({"ok":true,"report":report_value(&report),"identityCaveat":"Editable graph testimony, not authenticated seat identity or runtime liveness"}),
    )
}
pub(crate) async fn read(app: AppHandle, args: &Value) -> AppResult<Value> {
    let graph = graph_id(args)?;
    if args
        .as_object()
        .is_none_or(|v| v.keys().any(|k| k != "graph_id" && k != "graphId"))
    {
        return Err(AppError::validation(
            "agent_status accepts only graph identity",
        ));
    }
    let coordinator = app.state::<GraphPersistenceCoordinator>();
    let lease = coordinator
        .acquire_hot_write(&graph)
        .await
        .map_err(AppError::storage)?;
    lease.declare_rdf_read_only();
    let (dir, record) = crate::graph_record_store::read_graph_record_no_heal(&app, &graph)?;
    let incarnation = record
        .incarnation_id
        .ok_or_else(|| AppError::conflict("missing graph incarnation"))?;
    if !dir.join("store.oxigraph").is_dir() {
        return Err(AppError::storage(
            "Agent RDF store is unavailable; read does not create it",
        ));
    }
    let store = crate::rdf_service::open_graph_store(&dir).map_err(AppError::rdf)?;
    snapshot(&store, &graph, &incarnation, Utc::now())
}
pub(crate) async fn write(app: AppHandle, args: &Value) -> AppResult<Value> {
    let graph = graph_id(args)?;
    let principal = crate::cell_graph_boundary::current_cell_lease().map(|lease| lease.principal);
    let mut value = args.clone();
    if let Some(object) = value.as_object_mut() {
        object.remove("graph_id");
        object.remove("graphId");
    }
    let input: Input =
        serde_json::from_value(value).map_err(|e| AppError::validation(e.to_string()))?;
    validate(&input)?;
    let coordinator = app.state::<GraphPersistenceCoordinator>();
    let _lease = coordinator
        .acquire_hot_write(&graph)
        .await
        .map_err(AppError::storage)?;
    crate::restore_guard::require_no_active_restore(&app, &graph).map_err(AppError::conflict)?;
    let (dir, record) = crate::graph_record_store::read_graph_record_no_heal(&app, &graph)?;
    let incarnation = record
        .incarnation_id
        .ok_or_else(|| AppError::conflict("missing graph incarnation"))?;
    let store = crate::rdf_service::open_graph_store(&dir).map_err(AppError::rdf)?;
    let result = publish(&store, &graph, &incarnation, input, principal, Utc::now())?;
    crate::graph_service::touch_graph_updated_at(&dir).map_err(AppError::storage)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn input(body: &str) -> Input {
        serde_json::from_value(json!({"graphIncarnation":"inc-1","agentId":"agent-0123456789abcdef","bodyId":body,"activity":"Examining a fixture","ttlSeconds":60,"origin":"self-reported"})).unwrap()
    }
    fn fixture() -> Store {
        let store = Store::new().unwrap();
        crate::rdf_query_service::execute_sparql_update(&store, "INSERT DATA { GRAPH <urn:mnemosyne:local:graph:lab:user:rdf> { <urn:sophia:agent:agent-0123456789abcdef> a <http://mnemosyne.dev/agent#Agent> } }").unwrap();
        store
    }
    #[test]
    fn agent_status_timestamp_roundtrip_offset_calendar_precision_and_expiry() {
        let store = fixture();
        let now = DateTime::parse_from_rfc3339("2026-09-08T23:00:00.123999999Z")
            .unwrap()
            .with_timezone(&Utc);
        publish(&store, "lab", "inc-1", input("clock"), None, now).unwrap();
        let report = reports(&store, "lab")
            .unwrap()
            .into_values()
            .next()
            .unwrap();
        assert_eq!(report.reported_at.nanosecond(), 123_000_000);
        assert_eq!(
            report.expires_at - report.reported_at,
            Duration::seconds(60)
        );
        let encoded = serde_json::to_value(&report).unwrap();
        assert_eq!(encoded["reportedAt"], "2026-09-08T23:00:00.123Z");
        let roundtrip: Report = serde_json::from_value(encoded.clone()).unwrap();
        assert_eq!(roundtrip.reported_at, report.reported_at);
        assert_eq!(roundtrip.expires_at, report.expires_at);
        let mut offset = encoded.clone();
        offset["reportedAt"] = json!("2026-09-09T00:00:00.123+01:00");
        let normalized: Report = serde_json::from_value(offset).unwrap();
        assert_eq!(normalized.reported_at, report.reported_at);
        assert_eq!(
            serde_json::to_value(normalized).unwrap()["reportedAt"],
            encoded["reportedAt"]
        );
        for invalid in [
            "2026-02-30T23:00:00.123Z",
            "2026-09-08T23:00:00.123+24:00",
            "2026-09-08T23:00:00.123+00:60",
            "2026-09-08T23:00:00.123-00:00",
            "2026-09-08T23:00:00.123000001Z",
            "2016-12-31T23:59:60Z",
            "0000-01-01T00:00:00Z",
        ] {
            let mut bad = encoded.clone();
            bad["reportedAt"] = json!(invalid);
            assert!(
                serde_json::from_value::<Report>(bad).is_err(),
                "accepted {invalid}"
            );
        }
        let recent = snapshot(
            &store,
            "lab",
            "inc-1",
            report.expires_at - Duration::nanoseconds(1),
        )
        .unwrap();
        let expired = snapshot(&store, "lab", "inc-1", report.expires_at).unwrap();
        assert_eq!(recent["agents"][0]["reports"][0]["freshness"], "recent");
        assert_eq!(recent["asOf"], "2026-09-08T23:01:00.122Z");
        assert_eq!(expired["agents"][0]["reports"][0]["freshness"], "expired");
        assert!(snapshot(
            &store,
            "lab",
            "inc-1",
            report.reported_at - Duration::milliseconds(1)
        )
        .is_err());
        // An actual native store -> report reader -> snapshot producer witness.
        // Printed only after the assertions above pass; no HTTP/auth claim.
        println!("AGENT_STATUS_NATIVE_RECENT={}", recent);
        println!("AGENT_STATUS_NATIVE_EXPIRED={}", expired);
        let mut bad = encoded;
        bad["expiresAt"] = json!("2026-09-08T23:01:00.124Z");
        let target = crate::rdf_authority::user_rdf_graph_iri("lab");
        let subject = key(&report);
        let literal = crate::rdf::sparql_string_literal(&serde_json::to_string(&bad).unwrap());
        let update=format!("DELETE WHERE {{ GRAPH <{target}> {{ <{subject}> <{REPORT}> ?v }} }}; INSERT DATA {{ GRAPH <{target}> {{ <{subject}> <{REPORT}> {literal} }} }}");
        crate::rdf_query_service::execute_sparql_update(&store, &update).unwrap();
        let before = store.len().unwrap();
        assert!(snapshot(&store, "lab", "inc-1", report.reported_at).is_err());
        assert_eq!(before, store.len().unwrap());
    }
    #[test]
    fn agent_status_reports_expire_and_preserve_bodies_and_principals() {
        let store = fixture();
        let now = Utc::now();
        assert!(
            snapshot(&store, "lab", "inc-1", now).unwrap()["agents"][0]["reports"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        publish(&store, "lab", "inc-1", input("body-a"), None, now).unwrap();
        publish(
            &store,
            "lab",
            "inc-1",
            input("body-b"),
            Some("principal-a".into()),
            now,
        )
        .unwrap();
        publish(
            &store,
            "lab",
            "inc-1",
            input("body-b"),
            Some("principal-b".into()),
            now,
        )
        .unwrap();
        let view = snapshot(&store, "lab", "inc-1", now + Duration::seconds(60)).unwrap();
        let reports = view["agents"][0]["reports"].as_array().unwrap();
        assert_eq!(reports.len(), 3);
        assert!(reports.iter().all(|r| r["freshness"] == "expired"));
        assert!(reports.iter().any(|r| r["origin"] == "operator-reported"));
        assert!(
            snapshot(&store, "lab", "inc-2", now).unwrap()["agents"][0]["reports"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(snapshot(&store, "lab", "inc-1", now - Duration::seconds(1)).is_err());
    }
    #[test]
    fn agent_status_refuses_malformed_identity_unknown_fields_and_stale_scope() {
        let store = fixture();
        let now = Utc::now();
        let before = store.len().unwrap();
        assert!(publish(&store, "lab", "wrong", input("a"), None, now).is_err());
        let mut bad = input("a");
        bad.agent_id = "garden-agent-status".into();
        assert!(validate(&bad).is_err());
        bad = input("a");
        bad.activity = "x".repeat(2049);
        assert!(validate(&bad).is_err());
        bad = input("a");
        bad.ttl_seconds = 601;
        assert!(validate(&bad).is_err());
        bad = input("a");
        bad.agent_id = "agent-ffffffffffffffff".into();
        assert!(publish(&store, "lab", "inc-1", bad, None, now).is_err());
        let mut forged = serde_json::to_value(input("a")).unwrap();
        forged["ingressPrincipal"] = json!("forged");
        assert!(serde_json::from_value::<Input>(forged).is_err());
        assert_eq!(before, store.len().unwrap());
        assert!(graph_id(&json!({"graphId":"a","graph_id":"b"})).is_err());
        assert_eq!(
            graph_id(&json!({"graphId":"a","graph_id":"a"})).unwrap(),
            "a"
        );
    }
    #[test]
    fn agent_status_capacity_malformed_authority_and_cold_persistence() {
        let dir =
            std::env::temp_dir().join(format!("garden-agent-status-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let now = Utc::now();
        {
            let store = Store::open(&dir).unwrap();
            crate::rdf_query_service::execute_sparql_update(&store, "INSERT DATA { <urn:sophia:agent:agent-0123456789abcdef> a <http://mnemosyne.dev/agent#Agent> }").unwrap();
            publish(&store, "lab", "inc-1", input("persisted"), None, now).unwrap();
            store.flush().unwrap();
        }
        let store = Store::open(&dir).unwrap();
        assert_eq!(
            snapshot(&store, "lab", "inc-1", now).unwrap()["agents"][0]["reports"][0]["bodyId"],
            "persisted"
        );
        for index in 1..MAX_REPORTS {
            publish(
                &store,
                "lab",
                "inc-1",
                input(&format!("body-{index}")),
                None,
                now,
            )
            .unwrap();
        }
        let before = store.len().unwrap();
        assert!(publish(&store, "lab", "inc-1", input("overflow"), None, now).is_err());
        assert_eq!(before, store.len().unwrap());
        publish(&store, "lab", "inc-1", input("persisted"), None, now).unwrap();
        let target = crate::rdf_authority::user_rdf_graph_iri("lab");
        let bad =
            format!("INSERT DATA {{ GRAPH <{target}> {{ <urn:bad> <{REPORT}> \"malformed\" }} }}");
        crate::rdf_query_service::execute_sparql_update(&store, &bad).unwrap();
        let before = store.len().unwrap();
        assert!(snapshot(&store, "lab", "inc-1", now).is_err());
        assert_eq!(before, store.len().unwrap());
        // This isolated native fixture is retained for custody; no recursive cleanup.
    }
}
