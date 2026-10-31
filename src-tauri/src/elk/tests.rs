//! The ELK engine, run for real on QuickJS. No mocks.

use super::*;
use serde_json::json;
use sha2::{Digest, Sha256};

const GENEROUS: Duration = Duration::from_secs(60);

fn graph(nodes: usize, seed: u64) -> Value {
    let mut state = seed;
    let mut rand = move || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) as usize
    };
    let choice = choose(None, None, None).unwrap();
    let children: Vec<Value> = (0..nodes)
        .map(|i| json!({ "id": format!("n{i}"), "width": 120 + (rand() % 60), "height": 48 }))
        .collect();
    let edges: Vec<Value> = (1..nodes)
        .map(|i| json!({ "id": format!("e{i}"), "sources": [format!("n{}", rand() % i)], "targets": [format!("n{i}")] }))
        .collect();
    json!({ "id": "root", "layoutOptions": Value::Object(root_options(&choice)), "children": children, "edges": edges })
}

fn contains_key(value: &Value, key: &str) -> bool {
    match value {
        Value::Object(map) => map.contains_key(key) || map.values().any(|v| contains_key(v, key)),
        Value::Array(items) => items.iter().any(|v| contains_key(v, key)),
        _ => false,
    }
}

#[test]
fn the_vendored_elkjs_is_the_release_the_readme_names() {
    let readme = include_str!("../../vendor/elkjs/README.md");
    let digest: String =
        Sha256::digest(include_str!("../../vendor/elkjs/elk.bundled.js").as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
    assert!(
        readme.contains(&digest),
        "vendor/elkjs/README.md names sha256 {digest}"
    );
    assert_eq!(presets()["elkjs"], "0.12.0");
    assert!(readme.contains("elkjs@0.12.0"));
}

#[test]
fn presets_default_and_refuse_unknown_names() {
    assert_eq!(
        choose(None, None, None).unwrap(),
        Choice {
            style: "flow".into(),
            direction: Some("right".into()),
            spacing: "normal".into()
        }
    );
    assert_eq!(
        choose(Some("tree"), None, None)
            .unwrap()
            .direction
            .as_deref(),
        Some("down")
    );
    assert_eq!(
        choose(Some("grid"), Some("down"), None).unwrap().direction,
        None,
        "grid has no direction"
    );
    assert!(choose(Some("spiral"), None, None)
        .unwrap_err()
        .contains("style is one of"));
    assert!(choose(None, Some("sideways"), None)
        .unwrap_err()
        .contains("direction is one of"));
    assert!(choose(None, None, Some("roomy"))
        .unwrap_err()
        .contains("spacing is one of"));

    let tight = root_options(&choose(None, Some("down"), Some("tight")).unwrap());
    assert_eq!(tight["elk.direction"], "DOWN");
    assert_eq!(tight["elk.algorithm"], "layered");
    assert_eq!(tight["elk.spacing.nodeNode"].as_f64(), Some(40.0 * 0.6));
    assert_eq!(
        tight["elk.randomSeed"], 1,
        "only spacing keys are multiplied"
    );
    assert_eq!(frame_padding(), "[top=56,left=24,bottom=24,right=24]");
}

#[test]
fn a_layout_is_complete_deterministic_and_free_of_gwt_ids() {
    let g = graph(25, 7);
    let a = layout_blocking(g.clone(), GENEROUS).expect("layout");
    let b = layout_blocking(g, GENEROUS).expect("layout again");
    assert_eq!(a, b, "the same graph lays out the same, run after run");
    assert!(!contains_key(&a, "$H"), "GWT hash ids are stripped");
    for node in a["children"].as_array().unwrap() {
        assert!(
            node["x"].is_number() && node["y"].is_number(),
            "every node placed: {node}"
        );
    }
    for edge in a["edges"].as_array().unwrap() {
        let section = &edge["sections"][0];
        let mut route = vec![&section["startPoint"]];
        route.extend(section["bendPoints"].as_array().into_iter().flatten());
        route.push(&section["endPoint"]);
        for w in route.windows(2) {
            let (dx, dy) = (
                w[1]["x"].as_f64().unwrap() - w[0]["x"].as_f64().unwrap(),
                w[1]["y"].as_f64().unwrap() - w[0]["y"].as_f64().unwrap(),
            );
            assert!(
                dx.abs() < 0.01 || dy.abs() < 0.01,
                "flow routes are orthogonal: {edge}"
            );
        }
    }
}

#[test]
fn a_run_past_its_deadline_stops_cleanly_and_the_next_one_works() {
    let error = layout_blocking(graph(400, 3), Duration::from_millis(1)).unwrap_err();
    assert!(error.contains("took too long"), "{error}");
    let after = layout_blocking(graph(10, 5), GENEROUS).expect("the rebuilt engine lays out");
    assert_eq!(after["children"].as_array().unwrap().len(), 10);
}

#[test]
fn a_graph_elk_cannot_read_is_refused_and_the_engine_recovers() {
    let bad = json!({ "id": "root", "children": [{ "id": "a", "width": 10, "height": 10 }],
        "edges": [{ "id": "e", "sources": ["a"], "targets": ["nowhere"] }] });
    let error = layout_blocking(bad, GENEROUS).unwrap_err();
    assert!(error.starts_with("layout"), "{error}");
    layout_blocking(graph(5, 9), GENEROUS).expect("the next graph lays out");
}

#[test]
fn graphs_over_the_limits_never_reach_the_engine() {
    let big = graph(limits().nodes + 1, 1);
    let error = check_limits(&big).unwrap_err();
    assert!(error.contains("over the limit of 1000"), "{error}");
    let nested = json!({ "id": "root", "children": [{ "id": "f", "children": [{ "id": "a" }, { "id": "b" }], "edges": [{ "id": "e" }] }], "edges": [] });
    assert_eq!(count(&nested), (3, 1), "compounds and nested edges count");
}
