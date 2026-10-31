//! The `layout` move: a board (or part of it) as an ELK graph, and ELK's answer back as elements.
//!
//! The rules are the contract in `plans/2026-10-30-elk-layout/DESIGN.md`; Shrubbery implements the same ones for a
//! person's "Lay out", and both lay out `tests/fixtures/whiteboard/elk-fixture-board.excalidraw` to the same result.
//! ELK itself runs in `crate::elk`, never here and never inside a room transaction: `plan` builds the graph, the
//! caller has it laid out, and `apply` writes the answer.
//!
//! Vocabulary: a *unit* is what ELK moves as one node. An outermost Excalidraw group is one unit (its members keep
//! their places relative to each other); a frame with something in it is a compound node holding its children; any
//! other shape is a unit of its own. Arrows are never units: an arrow between two units is an ELK edge and takes ELK's
//! route; an arrow with one end in the set is re-routed straight, as a `move` does.

use super::scene::{
    bound_text, el_type, exit_point, is_bound_text, is_deleted, n, rect_of, reroute, s, touch, Ctx,
    Element, Rect, Scene,
};
use crate::elk::{self, Choice};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq)]
enum UnitKind {
    Element,
    Group,
    Frame,
}

#[derive(Clone, Debug)]
struct Unit {
    node: String,
    kind: UnitKind,
    /// The elements it moves (a group's members; the element itself; the frame itself).
    members: Vec<String>,
    rect: Rect,
    /// A frame unit's children (indexes into `Plan::units`), in order.
    children: Vec<usize>,
    parent: Option<usize>,
}

#[derive(Clone, Debug)]
struct Edge {
    arrow: String,
    source: usize,
    target: usize,
    start_element: String,
    end_element: String,
    label: Option<String>,
}

/// What one `layout` move will do to a scene: its units, edges and the arrows it re-routes.
#[derive(Clone, Debug)]
pub(crate) struct Plan {
    choice: Choice,
    units: Vec<Unit>,
    /// Top-level units in ELK's model order.
    top: Vec<usize>,
    edges: Vec<Edge>,
    /// Arrows bound to the set that are not edges (one end outside, or both ends on one unit).
    reroutes: Vec<String>,
    /// Each element of the set → its unit.
    unit_of: BTreeMap<String, usize>,
}

fn is_frame(el: &Element) -> bool {
    matches!(el_type(el), "frame" | "magicframe")
}

/// A shape ELK can place: not an arrow, not the text inside a container.
fn placeable(el: &Element) -> bool {
    el_type(el) != "arrow" && !is_bound_text(el)
}

fn outermost_group(el: &Element) -> Option<String> {
    el.get("groupIds")
        .and_then(Value::as_array)
        .and_then(|groups| groups.last())
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn union(rects: impl IntoIterator<Item = Rect>) -> Option<Rect> {
    let mut acc: Option<(f64, f64, f64, f64)> = None;
    for r in rects {
        acc = Some(match acc {
            None => (r.x, r.y, r.x + r.w, r.y + r.h),
            Some((a, b, c, d)) => (a.min(r.x), b.min(r.y), c.max(r.x + r.w), d.max(r.y + r.h)),
        });
    }
    acc.map(|(a, b, c, d)| Rect {
        x: a,
        y: b,
        w: c - a,
        h: d - b,
    })
}

/// Which elements a layout covers. `ids` are live element ids (refs already resolved); `None` = the whole board.
/// An arrow id is skipped, bound text means its container, a frame brings its children and a grouped shape brings
/// its outermost group.
fn the_set(scene: &Scene, ids: Option<&[String]>) -> BTreeSet<String> {
    let mut set: BTreeSet<String> = BTreeSet::new();
    match ids {
        None => {
            for (id, el) in scene.live() {
                if placeable(el) {
                    set.insert(id.clone());
                }
            }
            return set;
        }
        Some(ids) => {
            for id in ids {
                let Some(el) = scene.elements.get(id).filter(|e| !is_deleted(e)) else {
                    continue;
                };
                if el_type(el) == "arrow" {
                    continue;
                }
                if is_bound_text(el) {
                    if let Some(container) = s(el, "containerId")
                        .and_then(|c| scene.elements.get(c))
                        .filter(|c| !is_deleted(c) && placeable(c))
                    {
                        set.insert(s(container, "id").unwrap_or_default().to_string());
                    }
                    continue;
                }
                set.insert(id.clone());
            }
        }
    }
    loop {
        let mut more = BTreeSet::new();
        for id in &set {
            let el = &scene.elements[id];
            if is_frame(el) {
                for (kid, k) in scene.live() {
                    if s(k, "frameId") == Some(id.as_str()) && placeable(k) {
                        more.insert(kid.clone());
                    }
                }
            }
            if let Some(group) = outermost_group(el) {
                for (mate, m) in scene.live() {
                    if placeable(m) && outermost_group(m).as_deref() == Some(group.as_str()) {
                        more.insert(mate.clone());
                    }
                }
            }
        }
        let before = set.len();
        set.extend(more);
        if set.len() == before {
            return set;
        }
    }
}

/// Plan a layout of `ids` (or the whole board) on `scene`.
pub(crate) fn plan(scene: &Scene, ids: Option<&[String]>, choice: &Choice) -> Result<Plan, String> {
    let set = the_set(scene, ids);
    let mut units: Vec<Unit> = Vec::new();
    let mut unit_of: BTreeMap<String, usize> = BTreeMap::new();

    // Groups first: a group is one unit wherever its members sit.
    let mut group_unit: BTreeMap<String, usize> = BTreeMap::new();
    for id in &set {
        if let Some(group) = outermost_group(&scene.elements[id]) {
            let index = *group_unit.entry(group.clone()).or_insert_with(|| {
                units.push(Unit {
                    node: format!("group:{group}"),
                    kind: UnitKind::Group,
                    members: Vec::new(),
                    rect: Rect {
                        x: 0.0,
                        y: 0.0,
                        w: 0.0,
                        h: 0.0,
                    },
                    children: Vec::new(),
                    parent: None,
                });
                units.len() - 1
            });
            units[index].members.push(id.clone());
            unit_of.insert(id.clone(), index);
        }
    }
    // Then frames, then every other shape.
    for frames_pass in [true, false] {
        for id in &set {
            if unit_of.contains_key(id) || is_frame(&scene.elements[id]) != frames_pass {
                continue;
            }
            units.push(Unit {
                node: id.clone(),
                kind: if frames_pass {
                    UnitKind::Frame
                } else {
                    UnitKind::Element
                },
                members: vec![id.clone()],
                rect: Rect {
                    x: 0.0,
                    y: 0.0,
                    w: 0.0,
                    h: 0.0,
                },
                children: Vec::new(),
                parent: None,
            });
            unit_of.insert(id.clone(), units.len() - 1);
        }
    }
    for unit in &mut units {
        unit.rect = union(unit.members.iter().map(|m| rect_of(&scene.elements[m])))
            .expect("a unit has members");
    }
    // A shape in a frame that is in the set is that frame's child; a frame with no children is a plain shape.
    for index in 0..units.len() {
        if units[index].kind != UnitKind::Element {
            continue;
        }
        let frame = s(&scene.elements[&units[index].members[0]], "frameId").map(str::to_string);
        if let Some(parent) = frame.and_then(|f| unit_of.get(&f).copied()) {
            if units[parent].kind == UnitKind::Frame {
                units[index].parent = Some(parent);
                units[parent].children.push(index);
            }
        }
    }
    for unit in &mut units {
        if unit.kind == UnitKind::Frame && unit.children.is_empty() {
            unit.kind = UnitKind::Element;
        }
    }
    if units.len() < 2 {
        return Err("nothing to lay out: give at least two shapes".into());
    }

    // Model order at every level: the rect centre rounded half away from zero, across the flow first, then id.
    // An edge's place follows its ends' places in the graph as written (pre-order: a frame, then its children).
    let across_first = !matches!(choice.direction.as_deref(), Some("down" | "up"));
    let key = |u: &Unit| {
        let (cx, cy) = (
            (u.rect.x + u.rect.w / 2.0).round(),
            (u.rect.y + u.rect.h / 2.0).round(),
        );
        if across_first {
            (cy, cx)
        } else {
            (cx, cy)
        }
    };
    let by_key = |a: &usize, b: &usize| {
        let (ka, kb) = (key(&units[*a]), key(&units[*b]));
        ka.0.total_cmp(&kb.0)
            .then(ka.1.total_cmp(&kb.1))
            .then_with(|| units[*a].node.cmp(&units[*b].node))
    };
    let mut top: Vec<usize> = (0..units.len())
        .filter(|&u| units[u].parent.is_none())
        .collect();
    top.sort_by(by_key);
    let mut sorted_children: Vec<Vec<usize>> = units.iter().map(|u| u.children.clone()).collect();
    for children in &mut sorted_children {
        children.sort_by(by_key);
    }
    for (unit, children) in units.iter_mut().zip(sorted_children) {
        unit.children = children;
    }
    let mut rank: BTreeMap<usize, usize> = BTreeMap::new();
    let mut walk: Vec<usize> = top.iter().rev().copied().collect();
    while let Some(u) = walk.pop() {
        rank.insert(u, rank.len());
        walk.extend(units[u].children.iter().rev().copied());
    }

    // Edges and re-routes.
    let unit_for = |element: &str| -> Option<usize> {
        let el = scene.elements.get(element).filter(|e| !is_deleted(e))?;
        if is_bound_text(el) {
            return s(el, "containerId").and_then(|c| unit_of.get(c).copied());
        }
        unit_of.get(element).copied()
    };
    let mut edges = Vec::new();
    let mut reroutes = Vec::new();
    for (arrow_id, arrow) in scene.live() {
        if el_type(arrow) != "arrow" {
            continue;
        }
        let end = |key: &str| {
            arrow
                .get(key)
                .and_then(|b| b.get("elementId"))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        let (start_el, end_el) = (end("startBinding"), end("endBinding"));
        let (from, to) = (
            start_el.as_deref().and_then(unit_for),
            end_el.as_deref().and_then(unit_for),
        );
        match (from, to) {
            (Some(a), Some(b)) if a != b => edges.push(Edge {
                arrow: arrow_id.clone(),
                source: a,
                target: b,
                start_element: start_el.expect("bound"),
                end_element: end_el.expect("bound"),
                label: bound_text(scene, arrow)
                    .and_then(|t| s(t, "id"))
                    .map(str::to_string),
            }),
            (None, None) => {}
            _ => reroutes.push(arrow_id.clone()),
        }
    }
    edges.sort_by(|a, b| {
        rank[&a.source]
            .cmp(&rank[&b.source])
            .then(rank[&a.target].cmp(&rank[&b.target]))
            .then_with(|| a.arrow.cmp(&b.arrow))
    });
    Ok(Plan {
        choice: choice.clone(),
        units,
        top,
        edges,
        reroutes,
        unit_of,
    })
}

impl Plan {
    /// The ELK JSON graph (root options from the presets, edges on the root).
    /// One unit as an ELK node; a frame carries its children.
    fn node(&self, index: usize, padding: &str) -> Value {
        let unit = &self.units[index];
        let mut out = Map::new();
        out.insert("id".into(), json!(unit.node));
        out.insert("width".into(), json!(unit.rect.w));
        out.insert("height".into(), json!(unit.rect.h));
        if unit.kind == UnitKind::Frame {
            out.insert("layoutOptions".into(), json!({ "elk.padding": padding }));
            out.insert(
                "children".into(),
                Value::Array(
                    unit.children
                        .iter()
                        .map(|&c| self.node(c, padding))
                        .collect(),
                ),
            );
        }
        Value::Object(out)
    }

    /// The ELK JSON graph (root options from the presets, edges on the root).
    pub(crate) fn graph(&self, scene: &Scene) -> Value {
        let padding = elk::frame_padding();
        let edges: Vec<Value> = self
            .edges
            .iter()
            .map(|e| {
                let mut out = Map::new();
                out.insert("id".into(), json!(e.arrow));
                out.insert("sources".into(), json!([self.units[e.source].node]));
                out.insert("targets".into(), json!([self.units[e.target].node]));
                if let Some(label) = e.label.as_ref().and_then(|l| scene.elements.get(l)) {
                    out.insert(
                        "labels".into(),
                        json!([{
                            "id": s(label, "id").unwrap_or_default(),
                            "text": s(label, "text").unwrap_or_default(),
                            "width": n(label, "width"),
                            "height": n(label, "height"),
                        }]),
                    );
                }
                Value::Object(out)
            })
            .collect();
        json!({
            "id": "root",
            "layoutOptions": Value::Object(elk::root_options(&self.choice)),
            "children": self.top.iter().map(|&u| self.node(u, &padding)).collect::<Vec<_>>(),
            "edges": edges,
        })
    }
}

#[derive(Clone, Copy)]
struct Box2 {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

fn boxes(result: &Value) -> BTreeMap<String, Box2> {
    let mut out = BTreeMap::new();
    let mut stack: Vec<&Value> = result["children"]
        .as_array()
        .map(|c| c.iter().collect())
        .unwrap_or_default();
    while let Some(node) = stack.pop() {
        if let Some(id) = node["id"].as_str() {
            out.insert(
                id.to_string(),
                Box2 {
                    x: node["x"].as_f64().unwrap_or(0.0),
                    y: node["y"].as_f64().unwrap_or(0.0),
                    w: node["width"].as_f64().unwrap_or(0.0),
                    h: node["height"].as_f64().unwrap_or(0.0),
                },
            );
        }
        if let Some(children) = node["children"].as_array() {
            stack.extend(children);
        }
    }
    out
}

fn point(p: &Value) -> Option<(f64, f64)> {
    Some((p["x"].as_f64()?, p["y"].as_f64()?))
}

fn route_of(edge: &Value) -> Option<Vec<(f64, f64)>> {
    let section = edge["sections"].as_array()?.first()?;
    let mut route = vec![point(&section["startPoint"])?];
    for bend in section["bendPoints"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        route.push(point(bend)?);
    }
    route.push(point(&section["endPoint"])?);
    Some(route)
}

/// The point halfway along a polyline.
fn halfway(route: &[(f64, f64)]) -> (f64, f64) {
    let length: f64 = route
        .windows(2)
        .map(|w| (w[1].0 - w[0].0).hypot(w[1].1 - w[0].1))
        .sum();
    let mut left = length / 2.0;
    for w in route.windows(2) {
        let seg = (w[1].0 - w[0].0).hypot(w[1].1 - w[0].1);
        if seg >= left && seg > 0.0 {
            let t = left / seg;
            return (
                w[0].0 + (w[1].0 - w[0].0) * t,
                w[0].1 + (w[1].1 - w[0].1) * t,
            );
        }
        left -= seg;
    }
    route[0]
}

/// How far from axis-parallel a segment may be and still count as one (px).
const AXIS_TOLERANCE: f64 = 0.01;

/// A binding's fixed-point ratio: clamped to [0, 1], and never exactly the middle, which the pinned Excalidraw stores
/// as 0.5001 (`normalizeFixedPoint`, binding.ts).
fn ratio(v: f64, start: f64, size: f64) -> f64 {
    let r = if size <= 0.0 {
        0.5
    } else {
        ((v - start) / size).clamp(0.0, 1.0)
    };
    if (r - 0.5).abs() < 1e-4 {
        0.5001
    } else {
        r
    }
}

/// ELK routes an edge to its unit's node; when the arrow is bound to one member of a group that node is the group's
/// box, and the route may end beside the member. Snap that end onto the member: a horizontal last segment ends on the
/// near vertical side with its height clamped into the box (the bend before it follows, so the route keeps its right
/// angles; a two-point route becomes a Z through its middle); a vertical one likewise; a slanted one ends where the ray
/// toward its neighbour leaves the box.
fn snap_end(route: &mut Vec<(f64, f64)>, at_end: bool, r: &Rect, kind: &str) {
    let count = route.len();
    let (i, j) = if at_end {
        (count - 1, count - 2)
    } else {
        (0, 1)
    };
    let (p, q) = (route[i], route[j]);
    let horizontal = (p.1 - q.1).abs() <= AXIS_TOLERANCE;
    let vertical = (p.0 - q.0).abs() <= AXIS_TOLERANCE;
    if horizontal == vertical {
        route[i] = exit_point(kind, r, q);
        return;
    }
    let next = if horizontal {
        (
            if q.0 <= p.0 { r.x } else { r.x + r.w },
            p.1.max(r.y).min(r.y + r.h),
        )
    } else {
        (
            p.0.max(r.x).min(r.x + r.w),
            if q.1 <= p.1 { r.y } else { r.y + r.h },
        )
    };
    let shifted = if horizontal {
        next.1 != p.1
    } else {
        next.0 != p.0
    };
    if shifted && count == 2 {
        let mid = if horizontal {
            (p.0 + q.0) / 2.0
        } else {
            (p.1 + q.1) / 2.0
        };
        let bends = if horizontal {
            [(mid, q.1), (mid, next.1)]
        } else {
            [(q.0, mid), (next.0, mid)]
        };
        if at_end {
            route.splice(1..1, bends);
        } else {
            route.splice(1..1, [bends[1], bends[0]]);
        }
        let last = if at_end { route.len() - 1 } else { 0 };
        route[last] = next;
        return;
    }
    if shifted {
        route[j] = if horizontal {
            (q.0, next.1)
        } else {
            (next.0, q.1)
        };
    }
    route[i] = next;
}

/// An element without the fields every write stamps, for "did anything change".
fn unstamped(el: &Element) -> Element {
    let mut out = el.clone();
    for key in ["version", "versionNonce", "updated"] {
        out.remove(key);
    }
    if let Some(Value::Object(custom)) = out.get_mut("customData") {
        custom.remove("sophia");
    }
    out
}

fn shift(el: &mut Element, dx: f64, dy: f64) {
    el.insert("x".into(), json!(n(el, "x") + dx));
    el.insert("y".into(), json!(n(el, "y") + dy));
}

/// Write ELK's answer for `plan` into `scene`. Units the answer does not name (added since the graph was built) stay
/// where they are; so do elements that are gone.
pub(crate) fn apply(scene: &mut Scene, ctx: &mut Ctx, plan: &Plan, result: &Value) {
    let before_elements = scene.elements.clone();
    let dirty_before = scene.dirty.clone();
    write_answer(scene, ctx, plan, result);
    // Only what changed is written: an element that ends where it was keeps its version and costs its peers nothing.
    let touched: Vec<String> = scene.dirty.iter().cloned().collect();
    for id in touched {
        let Some(old) = before_elements.get(&id) else {
            continue;
        };
        if scene.elements.get(&id).map(unstamped) == Some(unstamped(old)) {
            scene.elements.insert(id.clone(), old.clone());
            if !dirty_before.contains(&id) {
                scene.dirty.remove(&id);
            }
        }
    }
}

fn write_answer(scene: &mut Scene, ctx: &mut Ctx, plan: &Plan, result: &Value) {
    let placed = boxes(result);
    let Some(before) = union(plan.units.iter().map(|u| u.rect)) else {
        return;
    };
    let after_min = result["children"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|n| Some((n["x"].as_f64()?, n["y"].as_f64()?)))
        .fold(None, |acc: Option<(f64, f64)>, (x, y)| {
            Some(acc.map_or((x, y), |(ax, ay)| (ax.min(x), ay.min(y))))
        });
    let Some((rx, ry)) = after_min else {
        return;
    };
    let (dx, dy) = (before.x - rx, before.y - ry);

    // Units: every moved element, and the text inside it, goes by its unit's translation.
    for unit in &plan.units {
        let Some(b) = placed.get(&unit.node) else {
            continue;
        };
        let (x, y) = (b.x + dx, b.y + dy);
        let (tx, ty) = (x - unit.rect.x, y - unit.rect.y);
        for member in &unit.members {
            let Some(el) = scene
                .elements
                .get(member)
                .filter(|e| !is_deleted(e))
                .cloned()
            else {
                continue;
            };
            let linear = matches!(el_type(&el), "line" | "freedraw" | "arrow");
            match unit.kind {
                UnitKind::Frame => touch(ctx, scene, member, |f| {
                    f.insert("x".into(), json!(x));
                    f.insert("y".into(), json!(y));
                    f.insert("width".into(), json!(b.w));
                    f.insert("height".into(), json!(b.h));
                }),
                UnitKind::Element if !linear => touch(ctx, scene, member, |e| {
                    e.insert("x".into(), json!(x));
                    e.insert("y".into(), json!(y));
                }),
                _ => touch(ctx, scene, member, |e| shift(e, tx, ty)),
            }
            if let Some(text) = bound_text(scene, &el)
                .and_then(|t| s(t, "id"))
                .map(str::to_string)
            {
                touch(ctx, scene, &text, |t| shift(t, tx, ty));
            }
        }
    }

    // Edges: ELK's route, bindings re-aimed at where the route meets each shape.
    let routed: BTreeMap<String, Vec<(f64, f64)>> = result["edges"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| Some((e["id"].as_str()?.to_string(), route_of(e)?)))
        .collect();
    let label_boxes: BTreeMap<String, Box2> = result["edges"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let l = e["labels"].as_array()?.first()?;
            Some((
                e["id"].as_str()?.to_string(),
                Box2 {
                    x: l["x"].as_f64()?,
                    y: l["y"].as_f64()?,
                    w: l["width"].as_f64()?,
                    h: l["height"].as_f64()?,
                },
            ))
        })
        .collect();
    let flow = plan.choice.style == "flow";
    let mut unrouted = Vec::new();
    for edge in &plan.edges {
        if scene
            .elements
            .get(&edge.arrow)
            .filter(|a| !is_deleted(a))
            .is_none()
        {
            continue;
        }
        let Some(route) = routed.get(&edge.arrow) else {
            unrouted.push(edge.arrow.clone());
            continue;
        };
        let mut route: Vec<(f64, f64)> = route.iter().map(|(x, y)| (x + dx, y + dy)).collect();
        // An end bound to one member of a group lands on that member, not on the group's box.
        for (element, at_end) in [(&edge.end_element, true), (&edge.start_element, false)] {
            let grouped = plan
                .unit_of
                .get(element.as_str())
                .is_some_and(|&u| plan.units[u].kind == UnitKind::Group);
            if let Some(shape) = scene
                .elements
                .get(element.as_str())
                .filter(|e| grouped && !is_deleted(e))
            {
                if route.len() >= 2 {
                    snap_end(&mut route, at_end, &rect_of(shape), el_type(shape));
                }
            }
        }
        let (x0, y0) = route[0];
        let points: Vec<Value> = route.iter().map(|(x, y)| json!([x - x0, y - y0])).collect();
        let (min_x, max_x) = route
            .iter()
            .fold((f64::MAX, f64::MIN), |(a, b), p| (a.min(p.0), b.max(p.0)));
        let (min_y, max_y) = route
            .iter()
            .fold((f64::MAX, f64::MIN), |(a, b), p| (a.min(p.1), b.max(p.1)));
        let elbow = flow
            && route.len() >= 3
            && route.windows(2).all(|w| {
                (w[1].0 - w[0].0).abs() <= AXIS_TOLERANCE
                    || (w[1].1 - w[0].1).abs() <= AXIS_TOLERANCE
            });
        let arrow_now = scene.elements[&edge.arrow].clone();
        let binding = |key: &str, element: &str, p: (f64, f64), scene: &Scene| -> Value {
            match scene.elements.get(element).filter(|e| !is_deleted(e)) {
                Some(shape) => {
                    let r = rect_of(shape);
                    json!({
                        "elementId": element,
                        "mode": "orbit",
                        "fixedPoint": [ratio(p.0, r.x, r.w), ratio(p.1, r.y, r.h)],
                    })
                }
                // A shape gone since the plan: the binding stays as it was.
                None => arrow_now.get(key).cloned().unwrap_or(Value::Null),
            }
        };
        let start = binding("startBinding", &edge.start_element, route[0], scene);
        let end = binding(
            "endBinding",
            &edge.end_element,
            *route.last().expect("a route has two ends"),
            scene,
        );
        touch(ctx, scene, &edge.arrow, |a| {
            a.insert("x".into(), json!(x0));
            a.insert("y".into(), json!(y0));
            a.insert("points".into(), Value::Array(points));
            a.insert("width".into(), json!(max_x - min_x));
            a.insert("height".into(), json!(max_y - min_y));
            a.insert("roundness".into(), Value::Null);
            a.insert("startBinding".into(), start);
            a.insert("endBinding".into(), end);
            if elbow {
                a.insert("elbowed".into(), json!(true));
                a.insert("fixedSegments".into(), json!([]));
                a.insert("startIsSpecial".into(), json!(false));
                a.insert("endIsSpecial".into(), json!(false));
            } else {
                a.insert("elbowed".into(), json!(false));
            }
        });
        if let Some(label) = &edge.label {
            let centre = match label_boxes.get(&edge.arrow).filter(|_| flow) {
                Some(b) => (b.x + dx + b.w / 2.0, b.y + dy + b.h / 2.0),
                None => halfway(&route),
            };
            touch(ctx, scene, label, |t| {
                let (w, h) = (n(t, "width"), n(t, "height"));
                t.insert("x".into(), json!(centre.0 - w / 2.0));
                t.insert("y".into(), json!(centre.1 - h / 2.0));
            });
        }
    }

    // Arrows half in the set, edges ELK did not route (grid), and anything bound to a moved unit: straight, as `move`.
    let mut straight: BTreeSet<String> = plan.reroutes.iter().cloned().collect();
    straight.extend(unrouted);
    for arrow in straight {
        if scene
            .elements
            .get(&arrow)
            .filter(|a| !is_deleted(a))
            .is_some()
        {
            reroute(ctx, scene, &arrow);
        }
    }
}
