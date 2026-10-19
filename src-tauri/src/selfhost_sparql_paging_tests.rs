//! Paging past the SPARQL row cap, through the real MCP `sparql_query`
//! handler over a real Oxigraph store (no mocks). Named so the cells lane's
//! `selfhost` filter runs it.
//!
//! A truncated SELECT returns `next`, a signed cursor; the same query with
//! `cursor` returns the next page. A forged cursor, a cursor from another
//! query, and a cursor issued before a write to the graph are refused. The
//! caller's row ceiling stays the page size (public visitors: 1,000).
use serde_json::{json, Value};
use std::collections::BTreeSet;

const GRAPH: &str = "sparql-paging";
const QUERY: &str = "SELECT ?s WHERE { GRAPH ?g { ?s <urn:paging:p> ?o } }";

fn with_graph(test: impl FnOnce(crate::app_runtime::AppHandle)) {
    let _serial = crate::tauri_runtime::profile_env_serial()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let previous = std::env::var_os("GARDEN_PROFILE_DIR");
    let profile =
        std::env::temp_dir().join(format!("garden-sparql-paging-{}", uuid::Uuid::new_v4()));
    std::env::set_var("GARDEN_PROFILE_DIR", &profile);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let app = crate::tauri_runtime::build_mock_app_for_tests(true);
        crate::graph_service::create_graph_service(
            &app,
            crate::graph_service::CreateGraphInput {
                graph_id: Some(GRAPH.into()),
                title: "SPARQL paging specimen".into(),
                description: None,
                operation_id: None,
            },
        )
        .unwrap();
        test(app);
    }));
    match previous {
        Some(value) => std::env::set_var("GARDEN_PROFILE_DIR", value),
        None => std::env::remove_var("GARDEN_PROFILE_DIR"),
    }
    let _ = std::fs::remove_dir_all(&profile);
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// `count` triples `<urn:paging:s{i}> <urn:paging:p> "i"`, loaded in chunks
/// through the real `rdf_load` handler.
fn load_rows(app: &crate::app_runtime::AppHandle, from: usize, count: usize) {
    const CHUNK: usize = 40_000;
    let mut start = from;
    while start < from + count {
        let end = (start + CHUNK).min(from + count);
        let data = (start..end)
            .map(|i| format!("<urn:paging:s{i}> <urn:paging:p> \"{i}\" .\n"))
            .collect::<String>();
        crate::rdf_service::mcp_local_rdf_load(
            app.clone(),
            &json!({"graphId": GRAPH, "data": data, "format": "ntriples"}),
        )
        .unwrap();
        start = end;
    }
}

fn query(app: &crate::app_runtime::AppHandle, args: Value) -> crate::app_error::AppResult<Value> {
    crate::app_runtime::async_runtime::block_on(crate::rdf_service::mcp_local_sparql_query(
        app.clone(),
        &args,
    ))
}

fn public_query(
    app: &crate::app_runtime::AppHandle,
    args: Value,
) -> crate::app_error::AppResult<Value> {
    let visitor = crate::public_mode::PublicRequest {
        visitor_id: "anon:0123456789abcdef01234567".into(),
        ip: "203.0.113.7".parse().unwrap(),
        account_token: false,
        sparql: crate::public_mode::SparqlCeiling {
            timeout: std::time::Duration::from_secs(120),
            max_rows: crate::public_mode::DEFAULT_SPARQL_MAX_ROWS,
        },
    };
    crate::app_runtime::async_runtime::block_on(crate::public_mode::as_public_request(
        visitor,
        crate::rdf_service::mcp_local_sparql_query(app.clone(), &args),
    ))
}

fn subjects(page: &Value) -> Vec<String> {
    page["rows"]
        .as_array()
        .unwrap_or_else(|| panic!("no rows: {page}"))
        .iter()
        .map(|row| row["s"].as_str().unwrap().to_string())
        .collect()
}

fn next(page: &Value) -> String {
    page["next"]
        .as_str()
        .unwrap_or_else(|| panic!("a truncated page carries `next`: {}", page["warnings"]))
        .to_string()
}

#[test]
fn a_120k_row_result_paged_at_50k_is_the_full_set_exactly_once() {
    with_graph(|app| {
        load_rows(&app, 0, 120_000);
        let cap = crate::rdf_query_service::sparql_server_max_rows();
        assert_eq!(cap, 50_000, "the external cap is the 50,000-row default");

        let mut seen = BTreeSet::new();
        let mut total = 0usize;
        let mut sizes = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut args = json!({"graphId": GRAPH, "query": QUERY, "timeoutMs": 120_000});
            if let Some(cursor) = cursor.as_ref() {
                args["cursor"] = json!(cursor);
            }
            let page = query(&app, args).unwrap();
            let rows = subjects(&page);
            sizes.push(rows.len());
            total += rows.len();
            seen.extend(rows);
            match page["next"].as_str() {
                Some(next) => cursor = Some(next.to_string()),
                None => break,
            }
            assert!(sizes.len() < 10, "paging does not end: {sizes:?}");
        }
        assert_eq!(sizes, vec![50_000, 50_000, 20_000]);
        assert_eq!(total, 120_000, "every row once");
        assert_eq!(seen.len(), 120_000, "no duplicates");
    });
}

#[test]
fn forged_and_foreign_cursors_are_refused() {
    with_graph(|app| {
        load_rows(&app, 0, 2_500);
        let first = query(
            &app,
            json!({"graphId": GRAPH, "query": QUERY, "maxRows": 1_000}),
        )
        .unwrap();
        assert_eq!(subjects(&first).len(), 1_000);
        let cursor = next(&first);

        // The genuine cursor works.
        let second = query(
            &app,
            json!({"graphId": GRAPH, "query": QUERY, "cursor": cursor}),
        )
        .unwrap();
        assert_eq!(subjects(&second).len(), 1_000);

        // Garbage is refused.
        let garbage = query(
            &app,
            json!({"graphId": GRAPH, "query": QUERY, "cursor": "not-a-cursor"}),
        );
        assert!(garbage.is_err(), "garbage cursor accepted: {garbage:?}");

        // A cursor whose payload was edited (skip to row 2,000) is refused.
        let (prefix_and_payload, signature) = cursor.rsplit_once('.').unwrap();
        let (prefix, payload) = prefix_and_payload.split_once('.').unwrap();
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
        let mut decoded: Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).unwrap()).unwrap();
        decoded["o"] = json!(2_000);
        let forged = format!(
            "{prefix}.{}.{signature}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&decoded).unwrap())
        );
        let refused = query(
            &app,
            json!({"graphId": GRAPH, "query": QUERY, "cursor": forged}),
        );
        let error = refused.expect_err("forged cursor accepted");
        assert!(error.to_string().contains("cursor"), "{error}");

        // A cursor from another query is refused.
        let other = "SELECT ?s ?o WHERE { GRAPH ?g { ?s <urn:paging:p> ?o } }";
        let refused = query(
            &app,
            json!({"graphId": GRAPH, "query": other, "cursor": cursor}),
        );
        let error = refused.expect_err("cursor from another query accepted");
        assert!(error.to_string().contains("another query"), "{error}");
    });
}

#[test]
fn a_cursor_issued_before_a_write_is_refused_as_stale() {
    with_graph(|app| {
        load_rows(&app, 0, 2_500);
        let first = query(
            &app,
            json!({"graphId": GRAPH, "query": QUERY, "maxRows": 1_000}),
        )
        .unwrap();
        let cursor = next(&first);
        load_rows(&app, 2_500, 1);
        let refused = query(
            &app,
            json!({"graphId": GRAPH, "query": QUERY, "cursor": cursor}),
        );
        let error = refused.expect_err("stale cursor accepted after a write");
        assert!(error.to_string().contains("stale"), "{error}");
        // Starting over works.
        let fresh = query(
            &app,
            json!({"graphId": GRAPH, "query": QUERY, "maxRows": 1_000}),
        )
        .unwrap();
        assert_eq!(subjects(&fresh).len(), 1_000);
        assert!(fresh["next"].is_string());
    });
}

#[test]
fn a_public_visitor_pages_at_the_public_ceiling() {
    with_graph(|app| {
        load_rows(&app, 0, 2_500);
        let ceiling = crate::public_mode::DEFAULT_SPARQL_MAX_ROWS;
        let first = public_query(&app, json!({"graphId": GRAPH, "query": QUERY})).unwrap();
        let first_rows = subjects(&first);
        assert_eq!(first_rows.len(), ceiling);
        // Asking for more does not raise the page size.
        let second = public_query(
            &app,
            json!({"graphId": GRAPH, "query": QUERY, "cursor": next(&first), "maxRows": 50_000}),
        )
        .unwrap();
        assert_eq!(subjects(&second).len(), ceiling);
        // Nor does an owner's 50k cursor, presented by a visitor.
        let owner = query(
            &app,
            json!({"graphId": GRAPH, "query": QUERY, "maxRows": 500}),
        )
        .unwrap();
        let owner_next = query(
            &app,
            json!({"graphId": GRAPH, "query": QUERY, "maxRows": 50_000}),
        )
        .unwrap();
        assert!(
            owner_next["next"].is_null(),
            "2,500 rows fit one owner page"
        );
        let via_visitor = public_query(
            &app,
            json!({"graphId": GRAPH, "query": QUERY, "cursor": next(&owner), "maxRows": 50_000}),
        )
        .unwrap();
        assert_eq!(subjects(&via_visitor).len(), ceiling);
        // The visitor's three pages are the whole set, once.
        let third = public_query(
            &app,
            json!({"graphId": GRAPH, "query": QUERY, "cursor": next(&second)}),
        )
        .unwrap();
        assert!(third["next"].is_null());
        let mut all: Vec<String> = first_rows;
        all.extend(subjects(&second));
        all.extend(subjects(&third));
        let unique: BTreeSet<_> = all.iter().cloned().collect();
        assert_eq!((all.len(), unique.len()), (2_500, 2_500));
    });
}
