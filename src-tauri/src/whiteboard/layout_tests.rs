//! The `layout` move with real ELK on QuickJS, no mocks. The board is the shared fixture
//! `tests/fixtures/whiteboard/elk-fixture-board.excalidraw` (real elements from the pinned Excalidraw, also laid out
//! by Shrubbery's tests): a frame of three mechanisms, a group (a box and its caption), labelled arrows, an elbow
//! arrow and an arrow to a shape outside the selection.
//!
//! With `ELK_FIXTURE_OUT=<path>` the contract test writes the laid-out fixture there, for the cross-check with
//! Shrubbery's result.

use super::scene::{apply_moves, n, rect_of, s, Ctx, Element, Rect, Rng, Scene, LAYOUT_PENDING};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

const FIXTURE: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/whiteboard/elk-fixture-board.excalidraw"
));

fn fixture() -> Scene {
    let file: Value = serde_json::from_str(FIXTURE).expect("fixture parses");
    let mut map = BTreeMap::new();
    for el in file["elements"].as_array().expect("elements") {
        let el = el.as_object().expect("element").clone();
        map.insert(el["id"].as_str().unwrap().to_string(), el);
    }
    Scene::from_elements(map)
}

/// The fixture's selection: every live shape except "waking" (arrows and bound text are never named).
fn selection(scene: &Scene) -> Vec<String> {
    scene
        .live()
        .filter(|(id, el)| {
            id.as_str() != "waking"
                && s(el, "type") != Some("arrow")
                && !(s(el, "type") == Some("text") && s(el, "containerId").is_some())
        })
        .map(|(id, _)| id.clone())
        .collect()
}

struct Run {
    scene: Scene,
    layouts: BTreeMap<usize, Value>,
    graphs: Vec<Value>,
}

/// Run `moves` the way the handler does: each pending layout goes to the real engine, then the moves run again.
fn run(scene: &Scene, moves: Value) -> Result<Run, String> {
    let (titles, objects) = (BTreeMap::new(), BTreeMap::new());
    let mut layouts: BTreeMap<usize, Value> = BTreeMap::new();
    let mut graphs = Vec::new();
    loop {
        let mut out = scene.clone();
        let mut ctx = Ctx {
            actor: "tester",
            now: 1_760_000_100_000,
            rng: Rng(11),
            graph_id: "g",
            document_id: "board",
            titles: &titles,
            objects: &objects,
            wires: Vec::new(),
            layouts: &layouts,
            pending_layout: None,
        };
        let result = apply_moves(&mut out, moves.as_array().unwrap(), &mut ctx);
        match ctx.pending_layout.take() {
            Some(pending) => {
                assert!(result.unwrap_err().ends_with(LAYOUT_PENDING));
                graphs.push(pending.graph.clone());
                let answer = crate::elk::layout_blocking(pending.graph, Duration::from_secs(60))?;
                layouts.insert(pending.index, answer);
            }
            None => {
                result?;
                return Ok(Run {
                    scene: out,
                    layouts,
                    graphs,
                });
            }
        }
    }
}

fn el<'a>(scene: &'a Scene, id: &str) -> &'a Element {
    scene
        .elements
        .get(id)
        .unwrap_or_else(|| panic!("no element {id}"))
}

fn union(rects: &[Rect]) -> Rect {
    let x0 = rects.iter().map(|r| r.x).fold(f64::MAX, f64::min);
    let y0 = rects.iter().map(|r| r.y).fold(f64::MAX, f64::min);
    let x1 = rects.iter().map(|r| r.x + r.w).fold(f64::MIN, f64::max);
    let y1 = rects.iter().map(|r| r.y + r.h).fold(f64::MIN, f64::max);
    Rect {
        x: x0,
        y: y0,
        w: x1 - x0,
        h: y1 - y0,
    }
}

fn overlaps(a: &Rect, b: &Rect) -> bool {
    a.x < b.x + b.w - 0.01
        && b.x < a.x + a.w - 0.01
        && a.y < b.y + b.h - 0.01
        && b.y < a.y + a.h - 0.01
}

fn inside(inner: &Rect, outer: &Rect) -> bool {
    inner.x >= outer.x - 0.01
        && inner.y >= outer.y - 0.01
        && inner.x + inner.w <= outer.x + outer.w + 0.01
        && inner.y + inner.h <= outer.y + outer.h + 0.01
}

fn absolute_points(arrow: &Element) -> Vec<(f64, f64)> {
    let (x, y) = (n(arrow, "x"), n(arrow, "y"));
    arrow["points"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (x + p[0].as_f64().unwrap(), y + p[1].as_f64().unwrap()))
        .collect()
}

fn bound_text_of<'a>(scene: &'a Scene, container: &str) -> &'a Element {
    scene
        .live()
        .map(|(_, e)| e)
        .find(|e| s(e, "containerId") == Some(container))
        .unwrap_or_else(|| panic!("no text in {container}"))
}

const EDGES: [&str; 8] = [
    "a-cond", "a-cond-m", "a-disp", "a-disp-m", "a-latent", "a-repr", "a-repr-m", "a-rev",
];

/// The units' rects on `scene`, top-level ones only (the frame stands for its children).
fn top_rects(scene: &Scene) -> Vec<(String, Rect)> {
    let r = |id: &str| rect_of(el(scene, id));
    vec![
        ("latent".into(), r("latent")),
        ("dreamwork".into(), r("dreamwork")),
        ("revision".into(), r("revision")),
        (
            "group:g-manifest".into(),
            union(&[r("manifest"), r("remembered")]),
        ),
        ("mechanisms".into(), r("mechanisms")),
    ]
}

#[test]
fn the_fixture_lays_out_by_the_contract() {
    let before = fixture();
    let ids = selection(&before);
    let run = run(&before, json!([{ "op": "layout", "ids": ids }])).expect("layout");
    let (after, graph, answer) = (&run.scene, &run.graphs[0], &run.layouts[&0]);
    assert_eq!(run.graphs.len(), 1, "one layout move, one graph");

    // The graph: units, the frame as a compound, edges, labels, the presets' options.
    let top: Vec<&str> = graph["children"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    let set: BTreeSet<&str> = top.iter().copied().collect();
    assert_eq!(
        set,
        BTreeSet::from([
            "latent",
            "dreamwork",
            "revision",
            "group:g-manifest",
            "mechanisms"
        ])
    );
    let frame = graph["children"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "mechanisms")
        .unwrap();
    let kids: BTreeSet<&str> = frame["children"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        kids,
        BTreeSet::from(["condensation", "displacement", "representability"])
    );
    assert_eq!(
        frame["layoutOptions"]["elk.padding"],
        "[top=56,left=24,bottom=24,right=24]"
    );
    let edges: BTreeSet<&str> = graph["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        edges,
        BTreeSet::from(EDGES),
        "a-wake has one end outside the set: not an edge"
    );
    let label = |id: &str| {
        graph["edges"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["id"] == id)
            .unwrap()["labels"][0]["text"]
            .clone()
    };
    assert_eq!(label("a-latent"), "worked by");
    assert_eq!(label("a-rev"), "polishes");
    let choice = crate::elk::choose(None, None, None).unwrap();
    assert_eq!(
        graph["layoutOptions"],
        Value::Object(crate::elk::root_options(&choice))
    );
    // Model order: across the flow (y), then along it (x), rounded half away from zero, then id.
    let key = |id: &str| {
        let r = top_rects(&before)
            .into_iter()
            .find(|(n, _)| n == id)
            .unwrap()
            .1;
        (
            (r.y + r.h / 2.0).round(),
            (r.x + r.w / 2.0).round(),
            id.to_string(),
        )
    };
    let mut sorted = top.clone();
    sorted.sort_by(|a, b| key(a).partial_cmp(&key(b)).unwrap());
    assert_eq!(top, sorted, "top-level nodes in model order");

    // Anchored: the drawing keeps its top-left corner.
    let unit_rects = |sc: &Scene| {
        let mut all: Vec<Rect> = top_rects(sc).into_iter().map(|(_, r)| r).collect();
        all.extend(
            ["condensation", "displacement", "representability"].map(|id| rect_of(el(sc, id))),
        );
        union(&all)
    };
    let (b, a) = (unit_rects(&before), unit_rects(after));
    assert!(
        (a.x - b.x).abs() < 0.01 && (a.y - b.y).abs() < 0.01,
        "anchor {a:?} vs {b:?}"
    );

    // Outside the set nothing moves.
    assert_eq!(n(el(after, "waking"), "x"), n(el(&before, "waking"), "x"));
    assert_eq!(n(el(after, "waking"), "y"), n(el(&before, "waking"), "y"));

    // A group keeps its shape; a frame holds its children; nothing overlaps.
    for key in ["x", "y"] {
        let gap = |sc: &Scene| n(el(sc, "remembered"), key) - n(el(sc, "manifest"), key);
        assert!(
            (gap(after) - gap(&before)).abs() < 0.01,
            "the group moved as one ({key})"
        );
    }
    let frame_rect = rect_of(el(after, "mechanisms"));
    for kid in ["condensation", "displacement", "representability"] {
        assert!(
            inside(&rect_of(el(after, kid)), &frame_rect),
            "{kid} sits in its frame"
        );
    }
    let tops = top_rects(after);
    for (i, (na, ra)) in tops.iter().enumerate() {
        for (nb, rb) in &tops[i + 1..] {
            assert!(!overlaps(ra, rb), "{na} and {nb} overlap after layout");
        }
    }

    // Text inside a container moves with it.
    for container in ["latent", "dreamwork", "condensation", "manifest", "waking"] {
        for key in ["x", "y"] {
            let offset =
                |sc: &Scene| n(bound_text_of(sc, container), key) - n(el(sc, container), key);
            assert!(
                (offset(after) - offset(&before)).abs() < 0.01,
                "{container}'s label moved with it ({key})"
            );
        }
    }

    // Edges take ELK's routes: orthogonal, elbow when bent, bound at both ends to the same shapes.
    let delta = (a.x - answer_min(answer).0, a.y - answer_min(answer).1);
    for id in EDGES {
        let arrow = el(after, id);
        let pts = absolute_points(arrow);
        for w in pts.windows(2) {
            assert!(
                (w[1].0 - w[0].0).abs() <= 0.01 || (w[1].1 - w[0].1).abs() <= 0.01,
                "{id} is orthogonal: {pts:?}"
            );
        }
        assert_eq!(
            arrow["elbowed"],
            json!(pts.len() >= 3),
            "{id}: elbow exactly when bent"
        );
        if pts.len() >= 3 {
            assert_eq!(arrow["fixedSegments"], json!([]));
            assert_eq!(arrow["startIsSpecial"], json!(false));
        }
        assert_eq!(arrow["roundness"], Value::Null);
        let route = answer["edges"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["id"] == id)
            .unwrap();
        let last = pts.last().unwrap();
        if el(&before, id)["endBinding"]["elementId"] == "manifest" {
            // ELK routed it to the group's box; the end is snapped onto the member it is bound to.
            let r = rect_of(el(after, "manifest"));
            let on_side = ((last.0 - r.x).abs() < 0.01 || (last.0 - r.x - r.w).abs() < 0.01)
                && last.1 >= r.y - 0.01
                && last.1 <= r.y + r.h + 0.01;
            let on_top = ((last.1 - r.y).abs() < 0.01 || (last.1 - r.y - r.h).abs() < 0.01)
                && last.0 >= r.x - 0.01
                && last.0 <= r.x + r.w + 0.01;
            assert!(
                on_side || on_top,
                "{id} ends on manifest's own box: {last:?} vs {r:?}"
            );
        } else {
            let end = &route["sections"][0]["endPoint"];
            assert!(
                (last.0 - (end["x"].as_f64().unwrap() + delta.0)).abs() < 0.01,
                "{id} ends where ELK says"
            );
            assert!(
                (last.1 - (end["y"].as_f64().unwrap() + delta.1)).abs() < 0.01,
                "{id} ends where ELK says"
            );
        }
        for (key, original) in [("startBinding", &before), ("endBinding", &before)] {
            let binding = &arrow[key];
            assert_eq!(binding["mode"], "orbit", "{id} {key}");
            assert_eq!(
                binding["elementId"],
                el(original, id)[key]["elementId"],
                "{id} {key} keeps its shape"
            );
            for v in binding["fixedPoint"].as_array().unwrap() {
                let v = v.as_f64().unwrap();
                assert!((0.0..=1.0).contains(&v), "{id} {key} fixedPoint in [0, 1]");
                assert_ne!(
                    v, 0.5,
                    "{id} {key}: the pinned Excalidraw never stores exactly 0.5 (0.5001)"
                );
            }
        }
    }
    // A label sits on ELK's label box.
    for (arrow, box_of) in [("a-latent", "worked by"), ("a-rev", "polishes")] {
        let label_box = &answer["edges"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["id"] == arrow)
            .unwrap()["labels"][0];
        let text = bound_text_of(after, arrow);
        assert_eq!(text["text"], box_of);
        let centre = (
            n(text, "x") + n(text, "width") / 2.0,
            n(text, "y") + n(text, "height") / 2.0,
        );
        let want = (
            label_box["x"].as_f64().unwrap() + delta.0 + label_box["width"].as_f64().unwrap() / 2.0,
            label_box["y"].as_f64().unwrap()
                + delta.1
                + label_box["height"].as_f64().unwrap() / 2.0,
        );
        assert!(
            (centre.0 - want.0).abs() < 0.01 && (centre.1 - want.1).abs() < 0.01,
            "{arrow} label on its box"
        );
    }
    // The arrow to the shape outside is re-routed straight, as a move does.
    let wake = el(after, "a-wake");
    assert_eq!(wake["points"].as_array().unwrap().len(), 2);
    assert_eq!(wake["elbowed"], false);

    // Every written element is stamped.
    for id in ["latent", "mechanisms", "a-repr-m", "remembered"] {
        assert!(
            el(after, id)["version"].as_i64() > el(&before, id)["version"].as_i64(),
            "{id} version bumped"
        );
        assert_eq!(el(after, id)["customData"]["sophia"]["by"], "tester");
    }

    if let Ok(path) = std::env::var("ELK_FIXTURE_OUT") {
        let mut out = serde_json::Map::new();
        for (id, e) in &after.elements {
            out.insert(
                id.clone(),
                json!({
                    "x": e.get("x"), "y": e.get("y"), "width": e.get("width"), "height": e.get("height"),
                    "points": e.get("points"), "elbowed": e.get("elbowed"),
                    "startBinding": e.get("startBinding"), "endBinding": e.get("endBinding"),
                }),
            );
        }
        let doc = json!({
            "about": "elk-fixture-board.excalidraw after {op:\"layout\", ids: every live shape except \"waking\"}, default preset, laid out by gardend (QuickJS)",
            "graph": graph,
            "elements": out,
        });
        std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap())
            .expect("write ELK_FIXTURE_OUT");
    }
}

fn answer_min(answer: &Value) -> (f64, f64) {
    answer["children"]
        .as_array()
        .unwrap()
        .iter()
        .fold((f64::MAX, f64::MAX), |(x, y), c| {
            (
                x.min(c["x"].as_f64().unwrap()),
                y.min(c["y"].as_f64().unwrap()),
            )
        })
}

#[test]
fn a_layout_replays_to_the_same_board() {
    let before = fixture();
    let moves = json!([{ "op": "layout", "ids": selection(&before) }]);
    let a = run(&before, moves.clone()).unwrap().scene;
    let b = run(&before, moves).unwrap().scene;
    assert_eq!(
        a.elements, b.elements,
        "the same board and the same move give the same result"
    );
}

#[test]
fn the_write_pass_applies_the_answer_to_a_board_a_person_changed() {
    // ELK answers for the board as it was; by the write a person has moved one shape, deleted another and added a
    // third. The answer lands on what is still there; the deleted shape stays deleted, the new one stays put.
    let before = fixture();
    let moves = json!([{ "op": "layout" }]);
    let laid = run(&before, moves.clone()).unwrap();
    let mut changed = before.clone();
    changed
        .elements
        .get_mut("latent")
        .unwrap()
        .insert("x".into(), json!(-900.0));
    changed
        .elements
        .get_mut("revision")
        .unwrap()
        .insert("isDeleted".into(), json!(true));
    let mut late = changed.elements["waking"].clone();
    late.insert("id".into(), json!("late"));
    late.insert("x".into(), json!(3000.0));
    late.insert("y".into(), json!(3000.0));
    late.insert("boundElements".into(), json!([]));
    changed.elements.insert("late".into(), late);

    let (titles, objects) = (BTreeMap::new(), BTreeMap::new());
    let mut out = changed.clone();
    let mut ctx = Ctx {
        actor: "tester",
        now: 1_760_000_100_000,
        rng: Rng(11),
        graph_id: "g",
        document_id: "board",
        titles: &titles,
        objects: &objects,
        wires: Vec::new(),
        layouts: &laid.layouts,
        pending_layout: None,
    };
    apply_moves(&mut out, moves.as_array().unwrap(), &mut ctx).expect("the stored answer applies");
    assert!(
        ctx.pending_layout.is_none(),
        "an answered layout never asks ELK again"
    );
    assert_eq!(
        (n(el(&out, "late"), "x"), n(el(&out, "late"), "y")),
        (3000.0, 3000.0),
        "a shape ELK never saw stays put"
    );
    assert_eq!(
        el(&out, "revision")["version"],
        el(&changed, "revision")["version"],
        "a deleted shape is left alone"
    );
    // The moved shape takes its laid-out place relative to the rest.
    let rel = |sc: &Scene| n(el(sc, "latent"), "x") - n(el(sc, "dreamwork"), "x");
    assert!(
        (rel(&out) - rel(&laid.scene)).abs() < 0.01,
        "latent sits where ELK put it, relative to dreamwork"
    );
}

#[test]
fn add_connect_and_layout_draw_a_diagram_in_one_call() {
    let run = run(
        &Scene::default(),
        json!([
            { "op": "add", "ref": "w", "shape": "box", "text": "the dream-work" },
            { "op": "add", "ref": "c", "shape": "note", "text": "condensation" },
            { "op": "add", "ref": "d", "shape": "note", "text": "displacement" },
            { "op": "add", "ref": "m", "shape": "ellipse", "text": "manifest content" },
            { "op": "connect", "ref": "wc", "from": "w", "to": "c" },
            { "op": "connect", "ref": "wd", "from": "w", "to": "d", "label": "displaces" },
            { "op": "connect", "ref": "cm", "from": "c", "to": "m" },
            { "op": "connect", "ref": "dm", "from": "d", "to": "m" },
            { "op": "layout", "ids": ["w", "c", "d", "m"], "direction": "down" }
        ]),
    )
    .expect("one call draws and lays out");
    let shapes: Vec<Rect> = run
        .scene
        .live()
        .filter(|(_, e)| matches!(s(e, "type"), Some("rectangle" | "ellipse")))
        .map(|(_, e)| rect_of(e))
        .collect();
    assert_eq!(shapes.len(), 4);
    for (i, a) in shapes.iter().enumerate() {
        for b in &shapes[i + 1..] {
            assert!(!overlaps(a, b), "laid-out shapes do not overlap");
        }
    }
    let arrows: Vec<&Element> = run
        .scene
        .live()
        .filter(|(_, e)| s(e, "type") == Some("arrow"))
        .map(|(_, e)| e)
        .collect();
    assert_eq!(arrows.len(), 4);
    for arrow in arrows {
        for w in absolute_points(arrow).windows(2) {
            assert!(
                (w[1].0 - w[0].0).abs() <= 0.01 || (w[1].1 - w[0].1).abs() <= 0.01,
                "orthogonal"
            );
        }
    }
    assert_eq!(run.graphs[0]["layoutOptions"]["elk.direction"], "DOWN");
}

#[test]
fn every_style_lays_the_fixture_out_without_overlaps() {
    let before = fixture();
    for style in ["flow", "tree", "grid"] {
        let laid = run(
            &before,
            json!([{ "op": "layout", "ids": selection(&before), "style": style }]),
        )
        .unwrap_or_else(|e| panic!("{style}: {e}"));
        let tops = top_rects(&laid.scene);
        for (i, (na, ra)) in tops.iter().enumerate() {
            for (nb, rb) in &tops[i + 1..] {
                assert!(!overlaps(ra, rb), "{style}: {na} and {nb} overlap");
            }
        }
        if style != "flow" {
            for id in EDGES {
                assert_eq!(
                    el(&laid.scene, id)["elbowed"],
                    false,
                    "{style}: only flow makes elbows ({id})"
                );
            }
        }
        if style == "grid" {
            // Rectpacking routes nothing: every arrow is re-routed straight.
            for id in EDGES {
                assert_eq!(
                    el(&laid.scene, id)["points"].as_array().unwrap().len(),
                    2,
                    "grid: {id} straight"
                );
            }
        }
    }
}

#[test]
fn a_layout_move_refuses_bad_names_and_too_little() {
    let before = fixture();
    let error = |moves: Value| {
        run(&before, moves)
            .err()
            .unwrap_or_else(|| panic!("refused"))
    };
    assert!(error(json!([{ "op": "layout", "style": "spiral" }])).contains("style is one of"));
    // Radial is not offered: ELK's radial layout is for trees and stacks the rest of a board on one spot.
    assert!(error(json!([{ "op": "layout", "style": "radial" }]))
        .contains("style is one of flow, tree, grid"));
    assert!(
        error(json!([{ "op": "layout", "direction": "sideways" }])).contains("direction is one of")
    );
    assert!(error(json!([{ "op": "layout", "ids": ["latent"] }])).contains("nothing to lay out"));
    assert!(
        error(json!([{ "op": "layout", "ids": ["nope", "latent"] }]))
            .contains("no shape or ref 'nope'")
    );
    assert!(error(json!([{ "op": "layout", "shape": "box" }])).contains("unknown field"));
    // Naming the text inside a container means the container; naming an arrow names nothing.
    let label = s(bound_text_of(&before, "latent"), "id")
        .unwrap()
        .to_string();
    let laid = run(
        &before,
        json!([{ "op": "layout", "ids": [label, "a-latent", "dreamwork"] }]),
    )
    .unwrap();
    let units: BTreeSet<&str> = laid.graphs[0]["children"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(units, BTreeSet::from(["latent", "dreamwork"]));
}

#[test]
fn applying_an_answer_to_a_board_already_laid_out_writes_nothing() {
    let before = fixture();
    let moves = json!([{ "op": "layout", "ids": selection(&before) }]);
    let laid = run(&before, moves.clone()).unwrap();
    let mut settled = laid.scene.clone();
    settled.dirty.clear();
    let (titles, objects) = (BTreeMap::new(), BTreeMap::new());
    let mut ctx = Ctx {
        actor: "tester",
        now: 1_760_000_200_000,
        rng: Rng(12),
        graph_id: "g",
        document_id: "board",
        titles: &titles,
        objects: &objects,
        wires: Vec::new(),
        layouts: &laid.layouts,
        pending_layout: None,
    };
    let mut again = settled.clone();
    apply_moves(&mut again, moves.as_array().unwrap(), &mut ctx).expect("the answer applies again");
    // A group's translation is recomputed from its members' union, so it may come back one ulp off (and the arrows
    // snapped onto its member with it). Everything else is not written at all.
    for id in [
        "latent",
        "dreamwork",
        "revision",
        "mechanisms",
        "condensation",
        "displacement",
        "representability",
        "a-cond",
        "a-latent",
    ] {
        assert!(
            !again.dirty.contains(id),
            "{id} did not move and is not written"
        );
        assert_eq!(
            again.elements[id], settled.elements[id],
            "{id} keeps its version"
        );
    }
    for id in &again.dirty {
        let (a, b) = (&again.elements[id], &settled.elements[id]);
        for key in ["x", "y"] {
            assert!(
                (n(a, key) - n(b, key)).abs() < 1e-6,
                "{id} moved only by float noise ({key})"
            );
        }
    }
}
