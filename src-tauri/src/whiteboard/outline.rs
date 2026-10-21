//! The board's text faces, from one deterministic reading of a [`Scene`]:
//! the `shapes`/`arrows` JSON an agent reads, and the outline markdown that is
//! the saved record's `body` (so search, `read_document` and the semantic index
//! see the words). The same board always gives the same bytes.

use super::scene::{
    bound_text, color_name_of, is_bound_text, is_deleted, n, rect_of, s, shape_kind, Element, Scene,
};
use serde_json::{json, Value};

/// A visible reading of one live non-arrow, non-bound-text element.
#[derive(Clone)]
pub(crate) struct Shape {
    pub id: String,
    pub kind: &'static str,
    pub text: String,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub color: String,
    pub frame: Option<String>,
    pub group: Option<String>,
    pub link: Option<String>,
    pub link_title: Option<String>,
    pub by: Option<String>,
}

pub(crate) struct ArrowRead {
    pub id: String,
    pub from: Option<String>,
    pub to: Option<String>,
    pub from_point: (f64, f64),
    pub to_point: (f64, f64),
    pub label: Option<String>,
    pub style: String,
    pub head: bool,
}

fn flat(text: &str) -> String {
    text.split('\n')
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" / ")
}

/// What a person typed: `originalText`, which Excalidraw keeps unwrapped; `text`
/// is that text broken at the container's width.
fn typed_text(el: &Element) -> String {
    flat(s(el, "originalText").or_else(|| s(el, "text")).unwrap_or(""))
}

fn by_of(el: &Element) -> Option<String> {
    el.get("customData")
        .and_then(|c| c.get("sophia"))
        .and_then(|c| c.get("by"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn link_title_of(el: &Element) -> Option<String> {
    el.get("customData")
        .and_then(|c| c.get("mnemosyne"))
        .and_then(|c| c.get("title"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

pub(crate) fn shapes(scene: &Scene) -> Vec<Shape> {
    let mut out: Vec<Shape> = scene
        .live()
        .filter(|(_, el)| !is_bound_text(el) && s(el, "type") != Some("arrow"))
        .map(|(id, el)| {
            let kind = shape_kind(el);
            let text = if kind == "text" {
                typed_text(el)
            } else if kind == "frame" {
                s(el, "name").unwrap_or("").to_string()
            } else {
                bound_text(scene, el)
                    .map(typed_text)
                    .unwrap_or_default()
            };
            let r = rect_of(el);
            Shape {
                id: id.clone(),
                kind,
                text,
                x: r.x,
                y: r.y,
                w: r.w,
                h: r.h,
                color: color_name_of(el),
                frame: s(el, "frameId").map(str::to_string),
                group: el
                    .get("groupIds")
                    .and_then(Value::as_array)
                    .and_then(|g| g.last())
                    .and_then(Value::as_str)
                    .map(str::to_string),
                link: s(el, "link").map(str::to_string),
                link_title: link_title_of(el),
                by: by_of(el),
            }
        })
        .collect();
    out.sort_by(|a, b| order_key(a).cmp(&order_key(b)));
    out
}

/// Top-to-bottom in 40 px bands, then left-to-right, then id: the order a
/// person reads a board in, and a total order so the bytes are stable.
fn order_key(shape: &Shape) -> (i64, i64, i64, String) {
    (
        ((shape.y + 20.0) / 40.0).floor() as i64,
        shape.x.round() as i64,
        shape.y.round() as i64,
        shape.id.clone(),
    )
}

pub(crate) fn arrows(scene: &Scene) -> Vec<ArrowRead> {
    let mut out: Vec<ArrowRead> = scene
        .live()
        .filter(|(_, el)| s(el, "type") == Some("arrow"))
        .map(|(id, el)| {
            let bound = |key: &str| {
                el.get(key)
                    .and_then(|b| b.get("elementId"))
                    .and_then(Value::as_str)
                    .filter(|target| {
                        scene
                            .elements
                            .get(*target)
                            .map(|t| !is_deleted(t))
                            .unwrap_or(false)
                    })
                    .map(str::to_string)
            };
            let points = el.get("points").and_then(Value::as_array);
            let point = |i: usize| {
                let p = points.and_then(|p| p.get(i)).cloned().unwrap_or(json!([0, 0]));
                (
                    n(el, "x") + p.get(0).and_then(Value::as_f64).unwrap_or(0.0),
                    n(el, "y") + p.get(1).and_then(Value::as_f64).unwrap_or(0.0),
                )
            };
            let last = points.map_or(1, |p| p.len().saturating_sub(1));
            ArrowRead {
                id: id.clone(),
                from: bound("startBinding"),
                to: bound("endBinding"),
                from_point: point(0),
                to_point: point(last),
                label: bound_text(scene, el)
                    .map(typed_text)
                    .filter(|l| !l.is_empty()),
                style: s(el, "strokeStyle").unwrap_or("solid").to_string(),
                head: !el.get("endArrowhead").map(Value::is_null).unwrap_or(true),
            }
        })
        .collect();
    out.sort_by(|a, b| {
        (a.from_point.1.round() as i64, a.from_point.0.round() as i64, &a.id)
            .cmp(&(b.from_point.1.round() as i64, b.from_point.0.round() as i64, &b.id))
    });
    out
}

fn num(v: f64) -> Value {
    json!(v.round() as i64)
}

pub(crate) fn shape_json(shape: &Shape) -> Value {
    let mut v = json!({
        "id": shape.id, "kind": shape.kind, "text": shape.text,
        "x": num(shape.x), "y": num(shape.y), "w": num(shape.w), "h": num(shape.h),
        "color": shape.color,
    });
    let obj = v.as_object_mut().expect("object");
    if let Some(f) = &shape.frame {
        obj.insert("frame".into(), json!(f));
    }
    if let Some(g) = &shape.group {
        obj.insert("group".into(), json!(g));
    }
    if let Some(l) = &shape.link {
        obj.insert("link".into(), json!(l));
    }
    if let Some(t) = &shape.link_title {
        obj.insert("linkTitle".into(), json!(t));
    }
    if let Some(b) = &shape.by {
        obj.insert("by".into(), json!(b));
    }
    v
}

pub(crate) fn arrow_json(arrow: &ArrowRead) -> Value {
    let mut v = json!({
        "id": arrow.id,
        "from": arrow.from, "to": arrow.to,
        "style": arrow.style,
    });
    let obj = v.as_object_mut().expect("object");
    if arrow.from.is_none() {
        obj.insert("fromPoint".into(), json!({ "x": num(arrow.from_point.0), "y": num(arrow.from_point.1) }));
    }
    if arrow.to.is_none() {
        obj.insert("toPoint".into(), json!({ "x": num(arrow.to_point.0), "y": num(arrow.to_point.1) }));
    }
    if let Some(l) = &arrow.label {
        obj.insert("label".into(), json!(l));
    }
    if !arrow.head {
        obj.insert("head".into(), json!("none"));
    }
    v
}

fn label_of(shape: &Shape) -> String {
    let base = if shape.text.is_empty() {
        format!("untitled {}", shape.kind)
    } else {
        shape.text.clone()
    };
    base
}

fn line_of(shape: &Shape) -> String {
    let mut line = format!("- {} ({})", label_of(shape), shape.kind);
    if let Some(title) = &shape.link_title {
        line.push_str(&format!(" [link: {title}]"));
    } else if let Some(link) = &shape.link {
        line.push_str(&format!(" [link: {link}]"));
    }
    line
}

/// The outline markdown. Frames first (by position), each with the shapes it
/// holds; then the rest (under "Elsewhere" when there are frames). Arrows are
/// listed under their source shape; arrows with no source are listed on their own.
pub(crate) fn outline_markdown(title: &str, scene: &Scene) -> String {
    let all = shapes(scene);
    let arrows = arrows(scene);
    let mut out = format!("# {}\n", flat(title));
    if all.is_empty() && arrows.is_empty() {
        out.push_str("\n(empty board)\n");
        return out;
    }
    let name_of = |id: &str| -> String {
        all.iter()
            .find(|s| s.id == id)
            .map(label_of)
            .unwrap_or_else(|| "(removed)".to_string())
    };
    let arrow_lines = |from: &str| -> Vec<String> {
        arrows
            .iter()
            .filter(|a| a.from.as_deref() == Some(from))
            .map(|a| {
                let target = match &a.to {
                    Some(id) => name_of(id),
                    None => format!("({}, {})", a.to_point.0.round(), a.to_point.1.round()),
                };
                match &a.label {
                    Some(label) => format!("  - → {target} (via \"{label}\")"),
                    None => format!("  - → {target}"),
                }
            })
            .collect()
    };
    let list = |out: &mut String, shapes: &[&Shape]| {
        for shape in shapes {
            out.push_str(&line_of(shape));
            out.push('\n');
            for line in arrow_lines(&shape.id) {
                out.push_str(&line);
                out.push('\n');
            }
        }
    };
    let frames: Vec<&Shape> = all.iter().filter(|s| s.kind == "frame").collect();
    for frame in &frames {
        out.push_str(&format!("\n## Frame: {}\n", label_of(frame)));
        let inside: Vec<&Shape> = all
            .iter()
            .filter(|s| s.kind != "frame" && s.frame.as_deref() == Some(frame.id.as_str()))
            .collect();
        list(&mut out, &inside);
    }
    let frame_ids: Vec<&str> = frames.iter().map(|f| f.id.as_str()).collect();
    let loose: Vec<&Shape> = all
        .iter()
        .filter(|s| {
            s.kind != "frame"
                && !s
                    .frame
                    .as_deref()
                    .map(|f| frame_ids.contains(&f))
                    .unwrap_or(false)
        })
        .collect();
    if !loose.is_empty() {
        if !frames.is_empty() {
            out.push_str("\n## Elsewhere\n");
        } else {
            out.push('\n');
        }
        list(&mut out, &loose);
    }
    // Arrows that start at no shape.
    let dangling: Vec<&ArrowRead> = arrows.iter().filter(|a| a.from.is_none()).collect();
    for a in dangling {
        let target = match &a.to {
            Some(id) => name_of(id),
            None => format!("({}, {})", a.to_point.0.round(), a.to_point.1.round()),
        };
        let label = a.label.as_ref().map(|l| format!(" (via \"{l}\")")).unwrap_or_default();
        out.push_str(&format!(
            "- arrow from ({}, {}) → {target}{label}\n",
            a.from_point.0.round(),
            a.from_point.1.round()
        ));
    }
    out
}
