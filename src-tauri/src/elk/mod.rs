//! The board layout engine: ELK (the Eclipse Layout Kernel, as elkjs) on QuickJS.
//!
//! One engine for people and agents: Shrubbery runs the same `elkjs` release in a Web Worker, and both sides take
//! every ELK option from one presets file (`presets.json`, copied verbatim into Shrubbery). The engine lives on one
//! dedicated thread (`engine.rs`); callers hand it an ELK JSON graph and get the laid-out graph back. Nothing here
//! knows about boards: `whiteboard::layout` turns a board into a graph and the result back into elements.
//!
//! ELK is deterministic for a given graph: the same positions and routes on every run and in a fresh engine. Only
//! GWT's object hash ids (`$H`) leak into its output; [`layout`] strips them.

mod engine;
#[cfg(test)]
mod tests;

use serde_json::{Map, Value};
use std::sync::OnceLock;
use std::time::Duration;

const PRESETS: &str = include_str!("presets.json");

/// The presets (`presets.json`), parsed once.
pub(crate) fn presets() -> &'static Value {
    static PARSED: OnceLock<Value> = OnceLock::new();
    PARSED.get_or_init(|| serde_json::from_str(PRESETS).expect("elk/presets.json parses"))
}

/// What one layout may cost (`presets.json` `limits`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    pub nodes: usize,
    pub edges: usize,
    pub layout_moves_per_call: usize,
    pub deadline: Duration,
}

pub(crate) fn limits() -> Limits {
    let l = &presets()["limits"];
    let get = |key: &str| {
        l[key]
            .as_u64()
            .unwrap_or_else(|| panic!("presets limits.{key}")) as usize
    };
    Limits {
        nodes: get("nodes"),
        edges: get("edges"),
        layout_moves_per_call: get("layoutMovesPerCall"),
        deadline: Duration::from_millis(get("deadlineMs") as u64),
    }
}

/// A style, direction and spacing as the presets name them, resolved to their defaults.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Choice {
    pub style: String,
    /// `None` for a style that has no direction (`grid`).
    pub direction: Option<String>,
    pub spacing: String,
}

/// Check and default a style, direction and spacing. Unknown names are refused with the names that exist.
pub(crate) fn choose(
    style: Option<&str>,
    direction: Option<&str>,
    spacing: Option<&str>,
) -> Result<Choice, String> {
    let p = presets();
    let style = style
        .unwrap_or_else(|| p["defaultStyle"].as_str().expect("defaultStyle"))
        .to_string();
    if p["styles"].get(&style).is_none() {
        return Err(format!("style is one of {}", names(&p["styles"])));
    }
    if let Some(direction) = direction {
        if p["directions"].get(direction).is_none() {
            return Err(format!("direction is one of {}", names(&p["directions"])));
        }
    }
    let spacing = spacing.unwrap_or("normal").to_string();
    if p["spacing"].get(&spacing).is_none() {
        return Err(format!("spacing is one of {}", names(&p["spacing"])));
    }
    let directional = p["directional"]
        .as_array()
        .expect("directional")
        .iter()
        .any(|s| s.as_str() == Some(style.as_str()));
    let direction = directional.then(|| {
        direction.map(str::to_string).unwrap_or_else(|| {
            p["defaultDirection"][&style]
                .as_str()
                .expect("defaultDirection")
                .to_string()
        })
    });
    Ok(Choice {
        style,
        direction,
        spacing,
    })
}

fn names(map: &Value) -> String {
    map.as_object()
        .map(|m| m.keys().cloned().collect::<Vec<_>>().join(", "))
        .unwrap_or_default()
}

/// The root's `layoutOptions`: `common` ∪ `styles[style]` ∪ `elk.direction`, every spacing key multiplied.
pub(crate) fn root_options(choice: &Choice) -> Map<String, Value> {
    let p = presets();
    let mut options = p["common"].as_object().expect("common").clone();
    for (key, value) in p["styles"][&choice.style].as_object().expect("style") {
        options.insert(key.clone(), value.clone());
    }
    if let Some(direction) = &choice.direction {
        options.insert("elk.direction".into(), p["directions"][direction].clone());
    }
    let factor = p["spacing"][&choice.spacing]
        .as_f64()
        .expect("spacing factor");
    for key in p["spacingKeys"].as_array().expect("spacingKeys") {
        let key = key.as_str().expect("spacing key");
        if let Some(value) = options.get(key).and_then(Value::as_f64) {
            options.insert(key.into(), serde_json::json!(value * factor));
        }
    }
    options
}

/// `elk.padding` for a frame laid out as a compound node (`framePadding`; the top leaves room for its name).
pub(crate) fn frame_padding() -> String {
    let f = &presets()["framePadding"];
    let n = |key: &str| f[key].as_f64().expect("framePadding");
    format!(
        "[top={},left={},bottom={},right={}]",
        n("top"),
        n("left"),
        n("bottom"),
        n("right")
    )
}

/// Every node (compounds included) and every edge of an ELK graph, at any depth.
pub(crate) fn count(graph: &Value) -> (usize, usize) {
    let mut nodes = 0;
    let mut edges = graph["edges"].as_array().map_or(0, Vec::len);
    let mut stack: Vec<&Value> = graph["children"]
        .as_array()
        .map(|c| c.iter().collect())
        .unwrap_or_default();
    while let Some(node) = stack.pop() {
        nodes += 1;
        edges += node["edges"].as_array().map_or(0, Vec::len);
        if let Some(children) = node["children"].as_array() {
            stack.extend(children);
        }
    }
    (nodes, edges)
}

/// Refuse a graph over the limits before it reaches the engine.
pub(crate) fn check_limits(graph: &Value) -> Result<(usize, usize), String> {
    let (nodes, edges) = count(graph);
    let l = limits();
    if nodes > l.nodes {
        return Err(format!(
            "layout: {nodes} nodes is over the limit of {}; lay out a part of the board",
            l.nodes
        ));
    }
    if edges > l.edges {
        return Err(format!(
            "layout: {edges} arrows is over the limit of {}; lay out a part of the board",
            l.edges
        ));
    }
    Ok((nodes, edges))
}

/// Lay `graph` out (ELK JSON in, the laid-out ELK JSON out, `$H` stripped). Runs on the engine thread; the caller
/// waits without blocking its runtime. A run past `deadline` is stopped and the engine rebuilt for the next one.
pub(crate) async fn layout(graph: Value, deadline: Duration) -> Result<Value, String> {
    check_limits(&graph)?;
    let reply = engine::submit(graph, deadline)?;
    let mut out = reply
        .await
        .map_err(|_| "layout: the layout engine stopped".to_string())??;
    strip_hash_ids(&mut out);
    Ok(out)
}

/// [`layout`] for a caller outside any async runtime (tests, a blocking thread).
#[cfg(test)]
pub(crate) fn layout_blocking(graph: Value, deadline: Duration) -> Result<Value, String> {
    check_limits(&graph)?;
    let reply = engine::submit(graph, deadline)?;
    let mut out = reply
        .blocking_recv()
        .map_err(|_| "layout: the layout engine stopped".to_string())??;
    strip_hash_ids(&mut out);
    Ok(out)
}

/// Remove GWT's `$H` object ids, the only part of ELK's output that differs from run to run.
pub(crate) fn strip_hash_ids(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("$H");
            for v in map.values_mut() {
                strip_hash_ids(v);
            }
        }
        Value::Array(items) => {
            for v in items {
                strip_hash_ids(v);
            }
        }
        _ => {}
    }
}
