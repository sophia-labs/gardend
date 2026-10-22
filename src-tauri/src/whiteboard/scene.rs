//! The pure core of the whiteboard: a scene is a map of Excalidraw element
//! JSON, and a *move* (add, connect, update, move, delete, link, group) edits it.
//! Nothing here touches Yrs, the store or the clock: the caller hands in the
//! actor, the time and a random seed, which is also what makes a replayed call
//! produce the same ids.
//!
//! Geometry is approximate where the server cannot measure: text is sized as
//! `chars x fontSize x 0.6`, and the browser re-measures (`restoreElements` with
//! `refreshDimensions`) when it renders.

use super::links::{self, LinkTarget};
use crate::scene_wire::{SceneWireDraft, SceneWireEnd, SceneWireProvenance};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) type Element = Map<String, Value>;

const FONT_SIZE: f64 = 20.0;
const LINE_HEIGHT: f64 = 1.25;
/// Approximate glyph width as a fraction of the font size.
const CHAR_WIDTH: f64 = 0.6;
/// Excalidraw's binding gap: BASE_BINDING_GAP (5) + strokeWidth / 2 (thin strokes: 5.5).
const BIND_GAP: f64 = 5.5;
const DEFAULT_NEAR_GAP: f64 = 60.0;
const WRAP_CHARS: usize = 22;
/// Nunito, the clean built-in family closest to Sophia's text. Shrubbery's person defaults use the same id.
const FONT_FAMILY: i64 = 6;
/// Thin: Excalidraw's strokeWidth 1 (the workflow canvas draws 1.5).
const STROKE_WIDTH: i64 = 1;
/// Architect: Excalidraw's roughness 0 (no hand-drawn wobble).
const ROUGHNESS: i64 = 0;

/// The named palette: (name, stroke, fill), taken from the light Sophia tokens (`reference.css`): amber = ochre-600
/// on ochre-100, blue = cobalt-500 on cobalt-100, green = the accent (fern-600 on fern-100), red = carmine-700 on
/// carmine-100, violet = indigo-600 on indigo-100 (Sophia's info hue), grey = parchment-500 on parchment-100.
/// In dark mode Excalidraw's own canvas filter inverts these.
const PALETTE: &[(&str, &str, &str)] = &[
    ("yellow", "#d97706", "#fef3c7"),
    ("blue", "#1a5fa3", "#dae8f4"),
    ("green", "#326f4c", "#dbecde"),
    ("red", "#be123c", "#ffe4e6"),
    ("violet", "#4f46e5", "#e0e7ff"),
    ("grey", "#777269", "#eeeae2"),
];
/// Boards drawn before the Sophia palette used Excalidraw's open-color picks; they still read back by name.
const LEGACY_PALETTE: &[(&str, &str, &str)] = &[
    ("yellow", "#f08c00", "#ffec99"),
    ("blue", "#1971c2", "#a5d8ff"),
    ("green", "#2f9e44", "#b2f2bb"),
    ("red", "#e03131", "#ffc9c9"),
    ("violet", "#6741d9", "#d0bfff"),
    ("grey", "#868e96", "#dee2e6"),
];
/// Sophia ink (parchment-900). Boards drawn before it used Excalidraw's `#1e1e1e`, still read as "black".
const INK: &str = "#1d1c19";
const LEGACY_INK: &str = "#1e1e1e";

#[derive(Clone, Default)]
pub(crate) struct Scene {
    pub(crate) elements: BTreeMap<String, Element>,
    /// Ids written since the scene was loaded (the Y.Doc write set).
    pub(crate) dirty: BTreeSet<String>,
}

impl Scene {
    pub(crate) fn from_elements(elements: BTreeMap<String, Element>) -> Self {
        Self {
            elements,
            dirty: BTreeSet::new(),
        }
    }

    pub(crate) fn live(&self) -> impl Iterator<Item = (&String, &Element)> {
        self.elements.iter().filter(|(_, el)| !is_deleted(el))
    }
}

// ---------------------------------------------------------------- accessors

pub(crate) fn s<'a>(el: &'a Element, key: &str) -> Option<&'a str> {
    el.get(key).and_then(Value::as_str)
}

pub(crate) fn n(el: &Element, key: &str) -> f64 {
    el.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

pub(crate) fn is_deleted(el: &Element) -> bool {
    el.get("isDeleted")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

pub(crate) fn el_type(el: &Element) -> &str {
    s(el, "type").unwrap_or("")
}

fn id_of(el: &Element) -> String {
    s(el, "id").unwrap_or("").to_string()
}

/// Ids in `boundElements` of the given type.
pub(crate) fn bound_ids(el: &Element, kind: &str) -> Vec<String> {
    el.get("boundElements")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some(kind))
                .filter_map(|b| b.get("id").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// The live bound text element of a container (shape or arrow).
pub(crate) fn bound_text<'a>(scene: &'a Scene, el: &Element) -> Option<&'a Element> {
    bound_ids(el, "text")
        .into_iter()
        .filter_map(|id| scene.elements.get(&id))
        .find(|t| !is_deleted(t))
}

pub(crate) fn is_bound_text(el: &Element) -> bool {
    el_type(el) == "text" && s(el, "containerId").is_some()
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    fn cx(&self) -> f64 {
        self.x + self.w / 2.0
    }
    fn cy(&self) -> f64 {
        self.y + self.h / 2.0
    }
    fn overlaps(&self, other: &Rect, pad: f64) -> bool {
        self.x < other.x + other.w + pad
            && other.x < self.x + self.w + pad
            && self.y < other.y + other.h + pad
            && other.y < self.y + self.h + pad
    }
}

pub(crate) fn rect_of(el: &Element) -> Rect {
    let (x, y) = (n(el, "x"), n(el, "y"));
    if matches!(el_type(el), "arrow" | "line" | "freedraw") {
        if let Some(points) = el.get("points").and_then(Value::as_array) {
            let (mut min_x, mut min_y, mut max_x, mut max_y) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
            for p in points {
                let px = p.get(0).and_then(Value::as_f64).unwrap_or(0.0);
                let py = p.get(1).and_then(Value::as_f64).unwrap_or(0.0);
                min_x = min_x.min(px);
                min_y = min_y.min(py);
                max_x = max_x.max(px);
                max_y = max_y.max(py);
            }
            return Rect {
                x: x + min_x,
                y: y + min_y,
                w: max_x - min_x,
                h: max_y - min_y,
            };
        }
    }
    Rect {
        x,
        y,
        w: n(el, "width"),
        h: n(el, "height"),
    }
}

/// Bounding box of everything live (bound text and deleted elements ignored).
pub(crate) fn bounds(scene: &Scene) -> Option<Rect> {
    let mut acc: Option<(f64, f64, f64, f64)> = None;
    for (_, el) in scene.live() {
        if is_bound_text(el) {
            continue;
        }
        let r = rect_of(el);
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

// ------------------------------------------------------------------ palette

pub(crate) fn color_name_of(el: &Element) -> String {
    let bg = s(el, "backgroundColor").unwrap_or("transparent");
    let stroke = s(el, "strokeColor").unwrap_or(INK);
    if is_filled(el) {
        for (name, _, fill) in PALETTE.iter().chain(LEGACY_PALETTE) {
            if fill.eq_ignore_ascii_case(bg) {
                return (*name).to_string();
            }
        }
        return bg.to_string();
    }
    for (name, st, _) in PALETTE.iter().chain(LEGACY_PALETTE) {
        if st.eq_ignore_ascii_case(stroke) {
            return (*name).to_string();
        }
    }
    if stroke.eq_ignore_ascii_case(INK) || stroke.eq_ignore_ascii_case(LEGACY_INK) {
        return "black".to_string();
    }
    stroke.to_string()
}

fn is_filled(el: &Element) -> bool {
    let bg = s(el, "backgroundColor").unwrap_or("transparent");
    !(bg.is_empty() || bg == "transparent")
}

fn palette_pair(name: &str) -> Option<(&'static str, &'static str)> {
    PALETTE
        .iter()
        .find(|(n, _, _)| *n == name)
        .map(|(_, st, fill)| (*st, *fill))
}

fn valid_color(name: &str) -> Result<(), String> {
    if palette_pair(name).is_some() || name == "black" {
        return Ok(());
    }
    if name.starts_with('#') && matches!(name.len(), 4 | 7) {
        return Ok(());
    }
    Err(format!(
        "color '{name}' is not one of yellow, blue, green, red, violet, grey, black or a #hex"
    ))
}

/// The kind a reader sees: see FORMAT.md.
pub(crate) fn shape_kind(el: &Element) -> &'static str {
    match el_type(el) {
        "rectangle" => {
            if is_filled(el) {
                "note"
            } else {
                "box"
            }
        }
        "ellipse" => "ellipse",
        "diamond" => "diamond",
        "text" => "text",
        "arrow" => "arrow",
        "line" => "line",
        "frame" | "magicframe" => "frame",
        "image" => "image",
        "freedraw" => "freedraw",
        // An `embeddable` that points at a document or an object of the graph is a card; any other (a web address,
        // an artifact, no link) is drawn as a box.
        "embeddable" if card_target(el).is_some() => "card",
        _ => "box",
    }
}

/// What a card points at: the `link` of an `embeddable` that parses as a `mnemosyne://document|object` link.
pub(crate) fn card_target(el: &Element) -> Option<LinkTarget> {
    if el_type(el) != "embeddable" {
        return None;
    }
    match links::parse_link(s(el, "link")?)? {
        target @ (LinkTarget::Document { .. } | LinkTarget::Object { .. }) => Some(target),
        LinkTarget::Artifact { .. } => None,
    }
}

/// The wire mark of a promoted arrow: `customData.mnemosyneWire`.
pub(crate) fn wire_mark(el: &Element) -> Option<&Map<String, Value>> {
    el.get("customData")
        .and_then(|c| c.get("mnemosyneWire"))
        .and_then(Value::as_object)
}

// ----------------------------------------------------------------- random

/// splitmix64: a small deterministic generator seeded by the caller, so a
/// journaled call replays to the same ids.
pub(crate) struct Rng(pub u64);

impl Rng {
    pub(crate) fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A 21-character id from Excalidraw's nanoid alphabet.
    pub(crate) fn id(&mut self) -> String {
        const ALPHABET: &[u8] = b"_-0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
        (0..21)
            .map(|_| ALPHABET[(self.next() % 64) as usize] as char)
            .collect()
    }

    fn nonce(&mut self) -> i64 {
        (self.next() % 2_000_000_000) as i64
    }
}

// ------------------------------------------------------------- fractional index

const B62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// A fractional index greater than `max` (Excalidraw's `generateKeyBetween(max, null)`
/// for the keys it generates itself). Anything unparseable falls back to an
/// appended midpoint, which still sorts after `max`.
fn index_after(max: Option<&str>) -> String {
    let Some(max) = max.filter(|m| !m.is_empty()) else {
        return "a0".to_string();
    };
    let bytes = max.as_bytes();
    let head = bytes[0];
    if !(b'a'..=b'y').contains(&head) {
        return format!("{max}V");
    }
    let int_len = (head - b'a') as usize + 2;
    if bytes.len() < int_len {
        return format!("{max}V");
    }
    let mut digits: Vec<u8> = bytes[1..int_len].to_vec();
    let frac = &max[int_len..];
    let mut i = digits.len();
    loop {
        if i == 0 {
            // Carried out of the integer part: next head, all zeros.
            let new_head = head + 1;
            let new_len = (new_head - b'a') as usize + 2;
            let mut out = String::new();
            out.push(new_head as char);
            out.push_str(&"0".repeat(new_len - 1));
            return out;
        }
        i -= 1;
        let pos = B62.iter().position(|b| *b == digits[i]);
        match pos {
            Some(p) if p + 1 < B62.len() => {
                digits[i] = B62[p + 1];
                break;
            }
            Some(_) => digits[i] = B62[0],
            None => return format!("{max}V"),
        }
    }
    let _ = frac; // an incremented integer part needs no fractional part
    let mut out = String::new();
    out.push(head as char);
    out.push_str(&String::from_utf8_lossy(&digits));
    out
}

// -------------------------------------------------------------------- text

fn wrap(text: &str, max_chars: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut current = String::new();
        for word in paragraph.split_whitespace() {
            let len = current.chars().count();
            let wl = word.chars().count();
            if len > 0 && len + 1 + wl > max_chars {
                lines.push(std::mem::take(&mut current));
            }
            if !current.is_empty() {
                current.push(' ');
            }
            current.push_str(word);
        }
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn measure(lines: &[String]) -> (f64, f64) {
    let widest = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) as f64;
    (
        (widest * FONT_SIZE * CHAR_WIDTH).max(FONT_SIZE),
        lines.len() as f64 * FONT_SIZE * LINE_HEIGHT,
    )
}

// ---------------------------------------------------------------- writing

pub(crate) struct Ctx<'a> {
    pub actor: &'a str,
    pub now: u64,
    pub rng: Rng,
    pub graph_id: &'a str,
    /// The board's document id (a `wire` move needs it for the endpoint rule and the scene provenance).
    pub document_id: &'a str,
    /// Titles of the documents a `link` or `card` move may name (resolved by the caller).
    pub titles: &'a BTreeMap<String, String>,
    /// The typed objects a `link` or `card` move may name, by IRI (resolved by the caller with `emporium_describe`).
    pub objects: &'a BTreeMap<String, ObjectInfo>,
    /// OUTPUT: the wires the `wire` moves of this call decided on, in move order. The caller stores them with
    /// `scene_wire::create_scene_wires` after the scene is written. The pure core never touches the store.
    pub wires: Vec<SceneWireDraft>,
}

/// What the caller learned about a typed object before the pure core runs: the snapshot a link or card keeps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ObjectInfo {
    pub label: String,
    pub class_label: String,
    /// The graph the object lives in.
    pub graph_id: String,
}

/// The endpoint rule (SPRINT D9) for one end of an arrow, `element_id` being the shape the arrow is bound to (bound
/// text counts as its container). A shape whose `link` is a `mnemosyne://document` link of this graph is that
/// document; a card or a shape whose link is a `mnemosyne://object` link is the object's IRI (`label` from
/// `customData.mnemosyne`); any other shape is itself, a block of the board (`document_id` = the board,
/// `block_id` = the element id). Errors name the element when it is deleted, missing, or an arrow.
pub(crate) fn resolve_wire_end(
    scene: &Scene,
    ctx: &Ctx,
    element_id: &str,
) -> Result<SceneWireEnd, String> {
    let shape_id = wire_shape_id(scene, element_id)?;
    let el = &scene.elements[&shape_id];
    let snapshot = |key: &str| {
        el.get("customData")
            .and_then(|c| c.get("mnemosyne"))
            .and_then(|c| c.get(key))
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    match s(el, "link").and_then(links::parse_link) {
        Some(LinkTarget::Document {
            graph_id,
            document_id,
        }) if graph_id == ctx.graph_id => Ok(SceneWireEnd {
            document_id: Some(document_id),
            block_id: None,
            iri: None,
            graph_id: None,
            label: snapshot("title"),
        }),
        Some(LinkTarget::Object { graph_id, iri }) => Ok(SceneWireEnd {
            document_id: None,
            block_id: None,
            iri: Some(iri),
            graph_id: (graph_id != ctx.graph_id).then_some(graph_id),
            label: snapshot("label"),
        }),
        // No link, a web address, an artifact or another graph's document: the shape itself, a block of the board.
        _ => Ok(SceneWireEnd {
            document_id: Some(ctx.document_id.to_string()),
            block_id: Some(shape_id),
            iri: None,
            graph_id: None,
            label: None,
        }),
    }
}

/// The live non-arrow element an arrow end names; bound text counts as its container.
fn wire_shape_id(scene: &Scene, element_id: &str) -> Result<String, String> {
    let mut id = element_id.to_string();
    // A text bound to a container stands for the container (one hop: a container is never itself bound text).
    if let Some(container) = scene
        .elements
        .get(&id)
        .filter(|el| is_bound_text(el))
        .and_then(|el| s(el, "containerId"))
    {
        id = container.to_string();
    }
    match scene.elements.get(&id) {
        None => Err(format!("no element '{id}' on this board")),
        Some(el) if is_deleted(el) => Err(format!("'{id}' was deleted")),
        Some(el) if el_type(el) == "arrow" => Err(format!("'{id}' is an arrow, not a shape")),
        Some(_) => Ok(id),
    }
}

/// Undo the marks of the `wire` moves on `arrow_ids`: the stroke the arrow had, its label when the wire move gave
/// it one, and the mark itself. The caller uses it when the wires could not be stored after the scene was written,
/// so a board never keeps an arrow that looks like a wire without one behind it.
pub(crate) fn unmark_wires(scene: &mut Scene, ctx: &mut Ctx, arrow_ids: &[String]) {
    for arrow_id in arrow_ids {
        let Some(arrow) = scene.elements.get(arrow_id) else {
            continue;
        };
        let Some(mark) = wire_mark(arrow).cloned() else {
            continue;
        };
        let stroke_was = mark
            .get("strokeWas")
            .and_then(Value::as_str)
            .unwrap_or(INK)
            .to_string();
        let label_from_predicate = mark
            .get("labelFromPredicate")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        touch(ctx, scene, arrow_id, |a| {
            a.insert("strokeColor".into(), json!(stroke_was));
            let mut custom = a
                .get("customData")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            custom.remove("mnemosyneWire");
            a.insert("customData".into(), Value::Object(custom));
        });
        if label_from_predicate {
            // Removing the label cannot fail for an arrow.
            let _ = set_text(scene, ctx, arrow_id, "");
        }
    }
}

impl Ctx<'_> {
    fn stamp(&mut self, el: &mut Element) {
        let version = el.get("version").and_then(Value::as_i64).unwrap_or(0) + 1;
        el.insert("version".into(), json!(version));
        el.insert("versionNonce".into(), json!(self.rng.nonce()));
        el.insert("updated".into(), json!(self.now));
        let mut custom = el
            .get("customData")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        custom.insert("sophia".into(), json!({ "by": self.actor, "at": self.now }));
        el.insert("customData".into(), Value::Object(custom));
    }
}

fn base_element(
    ctx: &mut Ctx,
    scene: &Scene,
    kind: &str,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
) -> Element {
    let max_index = scene
        .elements
        .values()
        .filter_map(|e| s(e, "index"))
        .max()
        .map(str::to_string);
    let mut el = Element::new();
    let id = ctx.rng.id();
    let seed = (ctx.rng.next() % 2_000_000_000) as i64;
    el.insert("id".into(), json!(id));
    el.insert("type".into(), json!(kind));
    el.insert("x".into(), json!(x));
    el.insert("y".into(), json!(y));
    el.insert("width".into(), json!(w));
    el.insert("height".into(), json!(h));
    el.insert("angle".into(), json!(0));
    el.insert("strokeColor".into(), json!(INK));
    el.insert("backgroundColor".into(), json!("transparent"));
    el.insert("fillStyle".into(), json!("solid"));
    el.insert("strokeWidth".into(), json!(STROKE_WIDTH));
    el.insert("strokeStyle".into(), json!("solid"));
    el.insert("roughness".into(), json!(ROUGHNESS));
    el.insert("opacity".into(), json!(100));
    el.insert("groupIds".into(), json!([]));
    el.insert("frameId".into(), Value::Null);
    el.insert("index".into(), json!(index_after(max_index.as_deref())));
    el.insert("roundness".into(), Value::Null);
    el.insert("seed".into(), json!(seed));
    el.insert("version".into(), json!(0));
    el.insert("versionNonce".into(), json!(0));
    el.insert("isDeleted".into(), json!(false));
    el.insert("boundElements".into(), json!([]));
    el.insert("updated".into(), json!(ctx.now));
    el.insert("link".into(), Value::Null);
    el.insert("locked".into(), json!(false));
    ctx.stamp(&mut el);
    el
}

fn insert_element(scene: &mut Scene, el: Element) -> String {
    let id = id_of(&el);
    scene.dirty.insert(id.clone());
    scene.elements.insert(id.clone(), el);
    id
}

fn touch(ctx: &mut Ctx, scene: &mut Scene, id: &str, edit: impl FnOnce(&mut Element)) {
    if let Some(mut el) = scene.elements.remove(id) {
        edit(&mut el);
        ctx.stamp(&mut el);
        scene.elements.insert(id.to_string(), el);
        scene.dirty.insert(id.to_string());
    }
}

fn add_bound(el: &mut Element, kind: &str, id: &str) {
    let list = el
        .entry("boundElements".to_string())
        .or_insert_with(|| json!([]));
    if !list.is_array() {
        *list = json!([]);
    }
    let arr = list.as_array_mut().expect("array");
    if !arr
        .iter()
        .any(|b| b.get("id").and_then(Value::as_str) == Some(id))
    {
        arr.push(json!({ "type": kind, "id": id }));
    }
}

fn text_element(
    ctx: &mut Ctx,
    scene: &Scene,
    text: &str,
    lines: &[String],
    x: f64,
    y: f64,
    container: Option<&str>,
) -> Element {
    let (w, h) = measure(lines);
    let mut t = base_element(ctx, scene, "text", x, y, w, h);
    t.insert("text".into(), json!(lines.join("\n")));
    t.insert("originalText".into(), json!(text));
    t.insert("fontSize".into(), json!(FONT_SIZE));
    t.insert("fontFamily".into(), json!(FONT_FAMILY));
    t.insert(
        "textAlign".into(),
        json!(if container.is_some() {
            "center"
        } else {
            "left"
        }),
    );
    t.insert(
        "verticalAlign".into(),
        json!(if container.is_some() { "middle" } else { "top" }),
    );
    t.insert(
        "containerId".into(),
        container.map_or(Value::Null, |c| json!(c)),
    );
    t.insert("autoResize".into(), json!(true));
    t.insert("lineHeight".into(), json!(LINE_HEIGHT));
    t
}

fn apply_color(el: &mut Element, name: &str) {
    let filled = is_filled(el)
        || el_type(el) == "rectangle" && s(el, "backgroundColor") != Some("transparent");
    let (stroke, fill) = match name {
        "black" => (INK.to_string(), None),
        n if palette_pair(n).is_some() => {
            let (st, f) = palette_pair(n).expect("checked");
            (st.to_string(), Some(f.to_string()))
        }
        hex => (hex.to_string(), Some(hex.to_string())),
    };
    el.insert("strokeColor".into(), json!(stroke));
    if filled {
        if let Some(fill) = fill {
            el.insert("backgroundColor".into(), json!(fill));
        }
    }
}

// ------------------------------------------------------------------ routing

fn exit_point(kind: &str, r: &Rect, toward: (f64, f64)) -> (f64, f64) {
    let (cx, cy) = (r.cx(), r.cy());
    let (dx, dy) = (toward.0 - cx, toward.1 - cy);
    if dx == 0.0 && dy == 0.0 {
        return (cx, cy);
    }
    let (hw, hh) = (r.w / 2.0, r.h / 2.0);
    let t = match kind {
        "ellipse" => 1.0 / ((dx / hw).powi(2) + (dy / hh).powi(2)).sqrt(),
        "diamond" => 1.0 / (dx.abs() / hw + dy.abs() / hh),
        _ => {
            let tx = if dx == 0.0 {
                f64::INFINITY
            } else {
                hw / dx.abs()
            };
            let ty = if dy == 0.0 {
                f64::INFINITY
            } else {
                hh / dy.abs()
            };
            tx.min(ty)
        }
    };
    (cx + dx * t, cy + dy * t)
}

/// The arrow end point on `shape`'s outline, `BIND_GAP` outside it, on the
/// centre-to-centre line toward `other`.
fn outline_point(shape: &Element, other_center: (f64, f64)) -> (f64, f64) {
    let r = rect_of(shape);
    let (ex, ey) = exit_point(el_type(shape), &r, other_center);
    let (dx, dy) = (ex - r.cx(), ey - r.cy());
    let len = (dx * dx + dy * dy).sqrt();
    if len == 0.0 {
        return (ex, ey);
    }
    (ex + dx / len * BIND_GAP, ey + dy / len * BIND_GAP)
}

fn center_of(el: &Element) -> (f64, f64) {
    let r = rect_of(el);
    (r.cx(), r.cy())
}

fn binding_target(arrow: &Element, key: &str) -> Option<String> {
    arrow
        .get(key)
        .and_then(|b| b.get("elementId"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn absolute_point(arrow: &Element, index: usize) -> (f64, f64) {
    let p = arrow
        .get("points")
        .and_then(Value::as_array)
        .and_then(|pts| pts.get(index))
        .cloned()
        .unwrap_or(json!([0, 0]));
    (
        n(arrow, "x") + p.get(0).and_then(Value::as_f64).unwrap_or(0.0),
        n(arrow, "y") + p.get(1).and_then(Value::as_f64).unwrap_or(0.0),
    )
}

/// Re-route one arrow from its bound shapes (a straight edge-to-edge segment).
/// An unbound end keeps its absolute position. Moves the arrow's label too.
fn reroute(ctx: &mut Ctx, scene: &mut Scene, arrow_id: &str) {
    let Some(arrow) = scene.elements.get(arrow_id).cloned() else {
        return;
    };
    let start_shape = binding_target(&arrow, "startBinding")
        .and_then(|id| scene.elements.get(&id))
        .filter(|e| !is_deleted(e))
        .cloned();
    let end_shape = binding_target(&arrow, "endBinding")
        .and_then(|id| scene.elements.get(&id))
        .filter(|e| !is_deleted(e))
        .cloned();
    let last = arrow
        .get("points")
        .and_then(Value::as_array)
        .map_or(1, |p| p.len().saturating_sub(1));
    let old_start = absolute_point(&arrow, 0);
    let old_end = absolute_point(&arrow, last);
    let start_hint = end_shape.as_ref().map(center_of).unwrap_or(old_end);
    let end_hint = start_shape.as_ref().map(center_of).unwrap_or(old_start);
    let start = start_shape
        .as_ref()
        .map(|sh| outline_point(sh, start_hint))
        .unwrap_or(old_start);
    let end = end_shape
        .as_ref()
        .map(|sh| outline_point(sh, end_hint))
        .unwrap_or(old_end);
    let (dx, dy) = (end.0 - start.0, end.1 - start.1);
    let label_id = bound_text(scene, &arrow).map(id_of);
    touch(ctx, scene, arrow_id, |a| {
        a.insert("x".into(), json!(start.0));
        a.insert("y".into(), json!(start.1));
        a.insert("points".into(), json!([[0, 0], [dx, dy]]));
        a.insert("width".into(), json!(dx.abs()));
        a.insert("height".into(), json!(dy.abs()));
        // A tool re-route is a straight segment: a person's elbow arrow becomes one rather than a bent line with
        // two points.
        a.insert("elbowed".into(), json!(false));
        a.insert("roundness".into(), Value::Null);
    });
    if let Some(label_id) = label_id {
        let mid = ((start.0 + end.0) / 2.0, (start.1 + end.1) / 2.0);
        touch(ctx, scene, &label_id, |t| {
            let (w, h) = (n(t, "width"), n(t, "height"));
            t.insert("x".into(), json!(mid.0 - w / 2.0));
            t.insert("y".into(), json!(mid.1 - h / 2.0));
        });
    }
}

fn arrows_bound_to(scene: &Scene, shape_id: &str) -> Vec<String> {
    scene
        .live()
        .filter(|(_, el)| el_type(el) == "arrow")
        .filter(|(_, el)| {
            binding_target(el, "startBinding").as_deref() == Some(shape_id)
                || binding_target(el, "endBinding").as_deref() == Some(shape_id)
        })
        .map(|(id, _)| id.clone())
        .collect()
}

/// Center a shape's bound text inside it.
fn recenter_text(ctx: &mut Ctx, scene: &mut Scene, shape_id: &str) {
    let Some(shape) = scene.elements.get(shape_id).cloned() else {
        return;
    };
    let Some(text_id) = bound_text(scene, &shape).map(id_of) else {
        return;
    };
    let r = rect_of(&shape);
    touch(ctx, scene, &text_id, |t| {
        let (w, h) = (n(t, "width"), n(t, "height"));
        t.insert("x".into(), json!(r.cx() - w / 2.0));
        t.insert("y".into(), json!(r.cy() - h / 2.0));
    });
}

// ----------------------------------------------------------------- the moves

#[derive(Deserialize, Clone, Copy)]
#[serde(deny_unknown_fields)]
struct Pt {
    x: f64,
    y: f64,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(deny_unknown_fields)]
struct Sz {
    w: f64,
    h: f64,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(deny_unknown_fields)]
struct By {
    dx: f64,
    dy: f64,
}

#[derive(Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct Near {
    id: Option<String>,
    r#ref: Option<String>,
    side: String,
    gap: Option<f64>,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum Move {
    Add {
        r#ref: Option<String>,
        shape: String,
        text: Option<String>,
        at: Option<Pt>,
        near: Option<Near>,
        size: Option<Sz>,
        color: Option<String>,
    },
    Connect {
        r#ref: Option<String>,
        from: String,
        to: String,
        label: Option<String>,
        style: Option<String>,
        head: Option<String>,
    },
    Update {
        id: String,
        text: Option<String>,
        color: Option<String>,
        link: Option<String>,
    },
    Move {
        id: String,
        at: Option<Pt>,
        near: Option<Near>,
        by: Option<By>,
    },
    Delete {
        id: String,
    },
    Link {
        id: String,
        #[serde(rename = "documentId")]
        document_id: Option<String>,
        iri: Option<String>,
        url: Option<String>,
    },
    Group {
        ids: Vec<String>,
        frame: Option<String>,
    },
    /// A typed object (`iri`) or a document (`documentId`) as a live card: an `embeddable` element (SPRINT D7).
    Card {
        r#ref: Option<String>,
        iri: Option<String>,
        #[serde(rename = "documentId")]
        document_id: Option<String>,
        at: Option<Pt>,
        near: Option<Near>,
        size: Option<Sz>,
    },
    /// Promote the arrow `arrow` to a wire named `predicate` (SPRINT D9). The arrow is marked
    /// `customData.mnemosyneWire {wireId, predicate}`; the wire itself is a `SceneWireDraft` pushed to `Ctx::wires`.
    Wire {
        arrow: String,
        predicate: String,
    },
}

struct Refs(BTreeMap<String, String>);

impl Refs {
    /// A `ref` from this call, else a live element id.
    fn resolve(&self, scene: &Scene, name: &str) -> Result<String, String> {
        if let Some(id) = self.0.get(name) {
            return Ok(id.clone());
        }
        match scene.elements.get(name) {
            Some(el) if !is_deleted(el) => {
                if is_bound_text(el) {
                    let container = s(el, "containerId").unwrap_or("");
                    return Err(format!(
                        "'{name}' is the text inside '{container}'; name '{container}' instead"
                    ));
                }
                Ok(name.to_string())
            }
            Some(_) => Err(format!("'{name}' was deleted")),
            None => Err(format!("no shape or ref '{name}' on this board")),
        }
    }
}

/// Apply `moves` in order. On any error the caller discards the scene, which is
/// how a malformed move writes nothing. Returns the `ref` -> id map in creation order.
pub(crate) fn apply_moves(
    scene: &mut Scene,
    moves: &[Value],
    ctx: &mut Ctx,
) -> Result<Vec<(String, String)>, String> {
    let mut parsed = Vec::new();
    for (i, raw) in moves.iter().enumerate() {
        let mv: Move =
            serde_json::from_value(raw.clone()).map_err(|e| format!("move {}: {e}", i + 1))?;
        parsed.push(mv);
    }
    let mut refs = Refs(BTreeMap::new());
    let mut created: Vec<(String, String)> = Vec::new();
    for (i, mv) in parsed.into_iter().enumerate() {
        run_move(scene, mv, ctx, &mut refs, &mut created)
            .map_err(|e| format!("move {}: {e}", i + 1))?;
    }
    Ok(created)
}

fn run_move(
    scene: &mut Scene,
    mv: Move,
    ctx: &mut Ctx,
    refs: &mut Refs,
    created: &mut Vec<(String, String)>,
) -> Result<(), String> {
    match mv {
        Move::Add {
            r#ref,
            shape,
            text,
            at,
            near,
            size,
            color,
        } => {
            if at.is_some() && near.is_some() {
                return Err("give at or near, not both".into());
            }
            if let Some(color) = &color {
                valid_color(color)?;
            }
            if let Some(name) = &r#ref {
                if refs.0.contains_key(name) {
                    return Err(format!("ref '{name}' is used twice"));
                }
            }
            let id = add_shape(
                scene,
                ctx,
                refs,
                &shape,
                text.as_deref(),
                at,
                near,
                size,
                color.as_deref(),
            )?;
            let name = r#ref.unwrap_or_else(|| id.clone());
            refs.0.insert(name.clone(), id.clone());
            created.push((name, id));
        }
        Move::Connect {
            r#ref,
            from,
            to,
            label,
            style,
            head,
        } => {
            let from = refs.resolve(scene, &from)?;
            let to = refs.resolve(scene, &to)?;
            if from == to {
                return Err("an arrow needs two different shapes".into());
            }
            for id in [&from, &to] {
                let kind = el_type(&scene.elements[id]);
                if matches!(kind, "arrow" | "line" | "freedraw" | "frame") {
                    return Err(format!("'{id}' is a {kind}; arrows connect boxes, notes, ellipses, diamonds and text"));
                }
            }
            if let Some(style) = &style {
                if !matches!(style.as_str(), "solid" | "dashed") {
                    return Err("style is solid or dashed".into());
                }
            }
            if let Some(head) = &head {
                if !matches!(head.as_str(), "arrow" | "none") {
                    return Err("head is arrow or none".into());
                }
            }
            if let Some(name) = &r#ref {
                if refs.0.contains_key(name) {
                    return Err(format!("ref '{name}' is used twice"));
                }
            }
            let id = connect(
                scene,
                ctx,
                &from,
                &to,
                label.as_deref(),
                style.as_deref(),
                head.as_deref(),
                &created.iter().map(|(_, id)| id.clone()).collect(),
            );
            let name = r#ref.unwrap_or_else(|| id.clone());
            refs.0.insert(name.clone(), id.clone());
            created.push((name, id));
        }
        Move::Update {
            id,
            text,
            color,
            link,
        } => {
            let id = refs.resolve(scene, &id)?;
            if text.is_none() && color.is_none() && link.is_none() {
                return Err("update needs text, color or link".into());
            }
            if let Some(color) = &color {
                valid_color(color)?;
            }
            if let Some(color) = color {
                touch(ctx, scene, &id, |el| apply_color(el, &color));
            }
            if let Some(link) = link {
                set_link(
                    scene,
                    ctx,
                    &id,
                    if link.is_empty() { None } else { Some(link) },
                    None,
                );
            }
            if let Some(text) = text {
                set_text(scene, ctx, &id, &text)?;
            }
        }
        Move::Move { id, at, near, by } => {
            let id = refs.resolve(scene, &id)?;
            if [at.is_some(), near.is_some(), by.is_some()]
                .iter()
                .filter(|b| **b)
                .count()
                != 1
            {
                return Err("move needs exactly one of at, near, by".into());
            }
            let el = scene.elements[&id].clone();
            if matches!(el_type(&el), "arrow") {
                return Err("an arrow follows the shapes it is bound to; move a shape".into());
            }
            let r = rect_of(&el);
            let (nx, ny) = if let Some(p) = at {
                (p.x, p.y)
            } else if let Some(d) = by {
                (r.x + d.dx, r.y + d.dy)
            } else {
                let near = near.expect("checked");
                // Place as if new, ignoring the shape itself.
                let mut without = scene.clone();
                without.elements.remove(&id);
                place_near(&without, refs, &near, r.w, r.h)?
            };
            let (dx, dy) = (nx - r.x, ny - r.y);
            // The shape, its bound text and anything it contains move together.
            let mut family = vec![id.clone()];
            family.extend(bound_ids(&el, "text"));
            if el_type(&el) == "frame" {
                let kids: Vec<String> = scene
                    .live()
                    .filter(|(_, k)| s(k, "frameId") == Some(id.as_str()) && el_type(k) != "arrow")
                    .map(|(kid, _)| kid.clone())
                    .collect();
                family.extend(kids);
            }
            let mut moved_shapes = Vec::new();
            for member in family {
                if scene.elements.contains_key(&member) {
                    touch(ctx, scene, &member, |m| {
                        m.insert("x".into(), json!(n(m, "x") + dx));
                        m.insert("y".into(), json!(n(m, "y") + dy));
                    });
                    moved_shapes.push(member);
                }
            }
            let mut to_route = BTreeSet::new();
            for shape in &moved_shapes {
                to_route.extend(arrows_bound_to(scene, shape));
            }
            for arrow in to_route {
                reroute(ctx, scene, &arrow);
            }
        }
        Move::Delete { id } => {
            let id = refs.resolve(scene, &id)?;
            delete_cascade(scene, ctx, &id);
        }
        Move::Link {
            id,
            document_id,
            iri,
            url,
        } => {
            let id = refs.resolve(scene, &id)?;
            match (document_id, iri, url) {
                (Some(doc), None, None) => {
                    let title = ctx.titles.get(&doc).cloned();
                    let title =
                        title.ok_or_else(|| format!("document '{doc}' is not in this graph"))?;
                    set_link(
                        scene,
                        ctx,
                        &id,
                        Some(links::document_link(ctx.graph_id, &doc)),
                        Some(document_record(ctx.graph_id, &doc, &title)),
                    );
                }
                (None, Some(iri), None) => {
                    let info = ctx
                        .objects
                        .get(&iri)
                        .cloned()
                        .ok_or_else(|| format!("object '{iri}' is not in this graph"))?;
                    set_link(
                        scene,
                        ctx,
                        &id,
                        Some(links::object_link(&info.graph_id, &iri)),
                        Some(object_record(&iri, &info)),
                    );
                }
                (None, None, Some(url)) => {
                    if !(url.starts_with("http://") || url.starts_with("https://")) {
                        return Err("url must start with http:// or https://".into());
                    }
                    set_link(scene, ctx, &id, Some(url), None);
                }
                _ => return Err("link needs exactly one of documentId, iri, url".into()),
            }
        }
        Move::Group { ids, frame } => {
            if ids.is_empty() {
                return Err("group needs ids".into());
            }
            let mut members = Vec::new();
            for raw in &ids {
                let id = refs.resolve(scene, raw)?;
                members.push(id);
            }
            if let Some(title) = frame {
                make_frame(scene, ctx, &members, &title, created, refs);
            } else {
                let gid = ctx.rng.id();
                let mut all = BTreeSet::new();
                for id in &members {
                    all.insert(id.clone());
                    all.extend(bound_ids(&scene.elements[id], "text"));
                }
                for id in all {
                    touch(ctx, scene, &id, |el| {
                        let mut groups = el
                            .get("groupIds")
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default();
                        groups.push(json!(gid));
                        el.insert("groupIds".into(), Value::Array(groups));
                    });
                }
            }
        }
        Move::Card {
            r#ref,
            iri,
            document_id,
            at,
            near,
            size,
        } => {
            if at.is_some() && near.is_some() {
                return Err("give at or near, not both".into());
            }
            if let Some(name) = &r#ref {
                if refs.0.contains_key(name) {
                    return Err(format!("ref '{name}' is used twice"));
                }
            }
            let (link, record) = match (document_id, iri) {
                (Some(doc), None) => {
                    let title = ctx
                        .titles
                        .get(&doc)
                        .cloned()
                        .ok_or_else(|| format!("document '{doc}' is not in this graph"))?;
                    (
                        links::document_link(ctx.graph_id, &doc),
                        document_record(ctx.graph_id, &doc, &title),
                    )
                }
                (None, Some(iri)) => {
                    let info = ctx
                        .objects
                        .get(&iri)
                        .cloned()
                        .ok_or_else(|| format!("object '{iri}' is not in this graph"))?;
                    (
                        links::object_link(&info.graph_id, &iri),
                        object_record(&iri, &info),
                    )
                }
                _ => return Err("card needs exactly one of iri, documentId".into()),
            };
            let id = add_card(scene, ctx, refs, link, record, at, near, size)?;
            let name = r#ref.unwrap_or_else(|| id.clone());
            refs.0.insert(name.clone(), id.clone());
            created.push((name, id));
        }
        Move::Wire { arrow, predicate } => promote_arrow(scene, ctx, refs, &arrow, &predicate)?,
    }
    Ok(())
}

/// `customData.mnemosyne` of a document link or card (the snapshot a board keeps so it can name a gone target).
fn document_record(graph_id: &str, document_id: &str, title: &str) -> Value {
    json!({ "kind": "document", "graphId": graph_id, "id": document_id, "title": title })
}

/// `customData.mnemosyne` of an object link or card.
fn object_record(iri: &str, info: &ObjectInfo) -> Value {
    json!({
        "kind": "object", "graphId": info.graph_id, "iri": iri,
        "label": info.label, "classLabel": info.class_label,
    })
}

const CARD_WIDTH: f64 = 320.0;
const CARD_HEIGHT: f64 = 180.0;

/// A card: an `embeddable` with the link and its snapshot, no text, placed like `add_shape` places a shape.
#[allow(clippy::too_many_arguments)]
fn add_card(
    scene: &mut Scene,
    ctx: &mut Ctx,
    refs: &Refs,
    link: String,
    record: Value,
    at: Option<Pt>,
    near: Option<Near>,
    size: Option<Sz>,
) -> Result<String, String> {
    let (mut w, mut h) = (CARD_WIDTH, CARD_HEIGHT);
    if let Some(sz) = size {
        if !(sz.w > 0.0 && sz.h > 0.0 && sz.w <= 5000.0 && sz.h <= 5000.0) {
            return Err("size must be positive and at most 5000".into());
        }
        w = sz.w;
        h = sz.h;
    }
    let (x, y) = if let Some(p) = at {
        (p.x, p.y)
    } else if let Some(near) = &near {
        place_near(scene, refs, near, w, h)?
    } else {
        match bounds(scene) {
            None => (0.0, 0.0),
            Some(b) => (b.x + b.w + 80.0, b.y),
        }
    };
    let mut el = base_element(ctx, scene, "embeddable", x, y, w, h);
    el.insert("roundness".into(), json!({ "type": 3 }));
    el.insert("link".into(), json!(link));
    let mut custom = el
        .get("customData")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    custom.insert("mnemosyne".into(), record);
    el.insert("customData".into(), Value::Object(custom));
    Ok(insert_element(scene, el))
}

/// The `wire` move (SPRINT D9): mark the arrow and hand the wire to the caller in `ctx.wires`. Nothing is stored here.
fn promote_arrow(
    scene: &mut Scene,
    ctx: &mut Ctx,
    refs: &Refs,
    arrow: &str,
    predicate: &str,
) -> Result<(), String> {
    let arrow_id = refs.resolve(scene, arrow)?;
    let el = scene.elements[&arrow_id].clone();
    if el_type(&el) != "arrow" {
        return Err(format!("'{arrow_id}' is not an arrow"));
    }
    let predicate = predicate.trim();
    if predicate.is_empty() {
        return Err("wire needs a predicate".into());
    }
    // The cell turns the predicate into an IRI; a name that cannot be one (a label with spaces, say) is refused here,
    // before the arrow is marked, so a refused move writes nothing.
    if oxigraph::model::NamedNode::new(crate::rdf_workspace_terms::wire_predicate_uri(predicate))
        .is_err()
    {
        return Err(format!(
            "wire predicate '{predicate}' is not a valid name or IRI: use one word such as supports or condensesInto, or an absolute IRI"
        ));
    }
    if wire_mark(&el).is_some() {
        return Err(format!(
            "arrow '{arrow_id}' is already a wire: unwire it first"
        ));
    }
    let dangling =
        || format!("arrow '{arrow_id}' has a dangling end: bind both ends before making a wire");
    let mut ends = Vec::new();
    for key in ["startBinding", "endBinding"] {
        let target = binding_target(&el, key).ok_or_else(dangling)?;
        let live = scene
            .elements
            .get(&target)
            .map(|t| !is_deleted(t) && el_type(t) != "arrow")
            .unwrap_or(false);
        if !live {
            return Err(dangling());
        }
        ends.push(wire_shape_id(scene, &target)?);
    }
    let (source_id, target_id) = (ends[0].clone(), ends[1].clone());
    let source = resolve_wire_end(scene, ctx, &source_id)?;
    let target = resolve_wire_end(scene, ctx, &target_id)?;
    let wire_id = format!("wire-{}", ctx.rng.id());
    ctx.wires.push(SceneWireDraft {
        wire_id: wire_id.clone(),
        predicate: predicate.to_string(),
        source,
        target,
        scene: SceneWireProvenance {
            graph_id: ctx.graph_id.to_string(),
            document_id: ctx.document_id.to_string(),
            element_id: arrow_id.clone(),
            source_element_id: source_id,
            target_element_id: target_id,
            stable_key: format!("wb:{}:{arrow_id}", ctx.document_id),
        },
    });
    let has_label = bound_text(scene, &el)
        .map(|t| !s(t, "originalText").or_else(|| s(t, "text")).unwrap_or("").is_empty())
        .unwrap_or(false);
    let stroke_was = s(&el, "strokeColor").unwrap_or(INK).to_string();
    let accent = palette_pair("green").map(|(stroke, _)| stroke).unwrap_or(INK);
    touch(ctx, scene, &arrow_id, |a| {
        a.insert("strokeColor".into(), json!(accent));
        let mut custom = a
            .get("customData")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        custom.insert(
            "mnemosyneWire".into(),
            json!({
                "wireId": wire_id, "predicate": predicate,
                "strokeWas": stroke_was, "labelFromPredicate": !has_label,
            }),
        );
        a.insert("customData".into(), Value::Object(custom));
    });
    if !has_label {
        set_text(scene, ctx, &arrow_id, predicate)?;
    }
    Ok(())
}

fn set_link(
    scene: &mut Scene,
    ctx: &mut Ctx,
    id: &str,
    link: Option<String>,
    record: Option<Value>,
) {
    touch(ctx, scene, id, |el| {
        el.insert("link".into(), link.map_or(Value::Null, Value::String));
        let mut custom = el
            .get("customData")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        match record {
            Some(record) => {
                custom.insert("mnemosyne".into(), record);
            }
            None => {
                custom.remove("mnemosyne");
            }
        }
        el.insert("customData".into(), Value::Object(custom));
    });
}

/// Set the text of a shape (its bound text, created if absent), a text element,
/// an arrow's label, or a frame's name.
fn set_text(scene: &mut Scene, ctx: &mut Ctx, id: &str, text: &str) -> Result<(), String> {
    let el = scene.elements[id].clone();
    if card_target(&el).is_some() {
        return Err("a card has no text of its own: its words come from what it points at".into());
    }
    match el_type(&el) {
        "text" => {
            let lines = wrap(text, 60);
            let (w, h) = measure(&lines);
            touch(ctx, scene, id, |t| {
                t.insert("text".into(), json!(lines.join("\n")));
                t.insert("originalText".into(), json!(text));
                t.insert("width".into(), json!(w));
                t.insert("height".into(), json!(h));
            });
        }
        "frame" => touch(ctx, scene, id, |f| {
            f.insert("name".into(), json!(text));
        }),
        "freedraw" | "line" | "image" => {
            return Err(format!("a {} has no text", el_type(&el)));
        }
        _ => {
            let is_arrow = el_type(&el) == "arrow";
            let lines = wrap(text, WRAP_CHARS);
            let (tw, th) = measure(&lines);
            match bound_text(scene, &el).map(id_of) {
                Some(tid) if text.is_empty() => {
                    delete_one(scene, ctx, &tid);
                    touch(ctx, scene, id, |c| remove_bound(c, &tid));
                }
                Some(tid) => {
                    touch(ctx, scene, &tid, |t| {
                        t.insert("text".into(), json!(lines.join("\n")));
                        t.insert("originalText".into(), json!(text));
                        t.insert("width".into(), json!(tw));
                        t.insert("height".into(), json!(th));
                    });
                    if !is_arrow {
                        grow_to_fit(scene, ctx, id, tw, th);
                    }
                    recenter_text(ctx, scene, id);
                }
                None if text.is_empty() => {}
                None => {
                    let r = rect_of(&el);
                    let t = text_element(
                        ctx,
                        scene,
                        text,
                        &lines,
                        r.cx() - tw / 2.0,
                        r.cy() - th / 2.0,
                        Some(id),
                    );
                    let tid = insert_element(scene, t);
                    touch(ctx, scene, id, |c| add_bound(c, "text", &tid));
                    if !is_arrow {
                        grow_to_fit(scene, ctx, id, tw, th);
                        recenter_text(ctx, scene, id);
                    }
                }
            }
        }
    }
    Ok(())
}

fn remove_bound(el: &mut Element, id: &str) {
    if let Some(list) = el.get_mut("boundElements").and_then(Value::as_array_mut) {
        list.retain(|b| b.get("id").and_then(Value::as_str) != Some(id));
    }
}

/// A container that is too small for its new text grows (the browser may grow it more).
fn grow_to_fit(scene: &mut Scene, ctx: &mut Ctx, id: &str, tw: f64, th: f64) {
    let r = rect_of(&scene.elements[id]);
    let (need_w, need_h) = (tw + 32.0, th + 32.0);
    if r.w >= need_w && r.h >= need_h {
        return;
    }
    let (nw, nh) = (r.w.max(need_w), r.h.max(need_h));
    touch(ctx, scene, id, |c| {
        c.insert("width".into(), json!(nw));
        c.insert("height".into(), json!(nh));
        c.insert("x".into(), json!(r.cx() - nw / 2.0));
        c.insert("y".into(), json!(r.cy() - nh / 2.0));
    });
    let arrows = arrows_bound_to(scene, id);
    for a in arrows {
        reroute(ctx, scene, &a);
    }
}

fn delete_one(scene: &mut Scene, ctx: &mut Ctx, id: &str) {
    touch(ctx, scene, id, |el| {
        el.insert("isDeleted".into(), json!(true));
    });
}

/// Delete decision: deleting a shape tombstones it, its bound text, AND every
/// arrow bound to it (with the arrows' labels). Excalidraw's own UI only unbinds
/// them; for an agent "delete the box" is clearer when it takes its arrows along.
fn delete_cascade(scene: &mut Scene, ctx: &mut Ctx, id: &str) {
    let el = scene.elements[id].clone();
    let mut doomed = vec![id.to_string()];
    doomed.extend(bound_ids(&el, "text"));
    if !matches!(el_type(&el), "arrow") {
        for arrow in arrows_bound_to(scene, id) {
            doomed.push(arrow.clone());
            doomed.extend(bound_ids(&scene.elements[&arrow], "text"));
        }
    }
    if el_type(&el) == "frame" {
        // A frame's children stay; they just leave the frame.
        let kids: Vec<String> = scene
            .live()
            .filter(|(_, k)| s(k, "frameId") == Some(id))
            .map(|(k, _)| k.clone())
            .collect();
        for kid in kids {
            touch(ctx, scene, &kid, |k| {
                k.insert("frameId".into(), Value::Null);
            });
        }
    }
    // Arrows deleted with a shape leave the other end's boundElements.
    for d in &doomed {
        if el_type(&scene.elements[d]) == "arrow" {
            for key in ["startBinding", "endBinding"] {
                if let Some(other) = binding_target(&scene.elements[d], key) {
                    if !doomed.contains(&other) && scene.elements.contains_key(&other) {
                        let d = d.clone();
                        touch(ctx, scene, &other, |o| remove_bound(o, &d));
                    }
                }
            }
        }
    }
    if let Some(container) = s(&el, "containerId").map(str::to_string) {
        touch(ctx, scene, &container, |c| remove_bound(c, id));
    }
    for d in doomed {
        delete_one(scene, ctx, &d);
    }
}

// ------------------------------------------------------------------- adding

fn default_size(shape: &str, lines: &[String]) -> (f64, f64) {
    let (tw, th) = measure(lines);
    let has_text = lines.iter().any(|l| !l.is_empty());
    match shape {
        "frame" => (400.0, 300.0),
        "ellipse" | "diamond" => {
            if has_text {
                ((tw * 1.5 + 40.0).max(140.0), (th * 1.5 + 40.0).max(90.0))
            } else {
                (140.0, 90.0)
            }
        }
        "note" => ((tw + 32.0).max(160.0), (th + 32.0).max(100.0)),
        _ => ((tw + 40.0).max(140.0), (th + 32.0).max(70.0)),
    }
}

#[allow(clippy::too_many_arguments)]
fn add_shape(
    scene: &mut Scene,
    ctx: &mut Ctx,
    refs: &Refs,
    shape: &str,
    text: Option<&str>,
    at: Option<Pt>,
    near: Option<Near>,
    size: Option<Sz>,
    color: Option<&str>,
) -> Result<String, String> {
    let text = text.unwrap_or("");
    if !matches!(
        shape,
        "box" | "note" | "ellipse" | "diamond" | "text" | "frame"
    ) {
        return Err(format!(
            "shape '{shape}' is not one of box, note, ellipse, diamond, text, frame"
        ));
    }
    if let Some(sz) = size {
        if !(sz.w > 0.0 && sz.h > 0.0 && sz.w <= 5000.0 && sz.h <= 5000.0) {
            return Err("size must be positive and at most 5000".into());
        }
    }
    let wrap_at = if shape == "text" { 60 } else { WRAP_CHARS };
    let lines = wrap(text, wrap_at);
    let (mut w, mut h) = match shape {
        "text" => measure(&lines),
        other => default_size(other, &lines),
    };
    if let Some(sz) = size {
        w = sz.w;
        h = sz.h;
    }
    let (x, y) = if let Some(p) = at {
        (p.x, p.y)
    } else if let Some(near) = &near {
        place_near(scene, refs, near, w, h)?
    } else {
        match bounds(scene) {
            None => (0.0, 0.0),
            Some(b) => (b.x + b.w + 80.0, b.y),
        }
    };
    if shape == "text" {
        let el = text_element(ctx, scene, text, &lines, x, y, None);
        let mut el = el;
        if let Some(color) = color {
            apply_color(&mut el, color);
            el.insert("backgroundColor".into(), json!("transparent"));
        }
        return Ok(insert_element(scene, el));
    }
    let kind = match shape {
        "box" | "note" => "rectangle",
        "frame" => "frame",
        other => other,
    };
    let mut el = base_element(ctx, scene, kind, x, y, w, h);
    if matches!(shape, "box" | "note") {
        el.insert("roundness".into(), json!({ "type": 3 }));
    } else if shape == "diamond" {
        el.insert("roundness".into(), json!({ "type": 2 }));
    }
    if shape == "frame" {
        el.insert(
            "name".into(),
            json!(if text.is_empty() {
                Value::Null
            } else {
                json!(text)
            }),
        );
    }
    match shape {
        "note" => {
            let name = color.unwrap_or("yellow");
            // A note is a filled rectangle, so the fill is what makes it one.
            let (stroke, fill) = palette_pair(name).unwrap_or((name, name));
            el.insert("strokeColor".into(), json!(stroke));
            el.insert("backgroundColor".into(), json!(fill));
        }
        "frame" => {}
        _ => {
            if let Some(name) = color {
                apply_color(&mut el, name);
            }
        }
    }
    let id = insert_element(scene, el);
    if shape != "frame" && !text.is_empty() {
        let (tw, th) = measure(&lines);
        let t = text_element(
            ctx,
            scene,
            text,
            &lines,
            x + w / 2.0 - tw / 2.0,
            y + h / 2.0 - th / 2.0,
            Some(&id),
        );
        let tid = insert_element(scene, t);
        touch(ctx, scene, &id, |c| add_bound(c, "text", &tid));
    }
    Ok(id)
}

fn place_near(
    scene: &Scene,
    refs: &Refs,
    near: &Near,
    w: f64,
    h: f64,
) -> Result<(f64, f64), String> {
    let anchor_name = near
        .id
        .as_ref()
        .or(near.r#ref.as_ref())
        .ok_or("near needs an id or a ref")?;
    let anchor_id = refs.resolve(scene, anchor_name)?;
    let a = rect_of(&scene.elements[&anchor_id]);
    let gap = near.gap.unwrap_or(DEFAULT_NEAR_GAP);
    let (mut x, mut y, step_x, step_y, push_x, push_y) = match near.side.as_str() {
        "right" => (a.x + a.w + gap, a.cy() - h / 2.0, 0.0, h + 24.0, 1.0, 0.0),
        "left" => (a.x - gap - w, a.cy() - h / 2.0, 0.0, h + 24.0, -1.0, 0.0),
        "below" => (a.cx() - w / 2.0, a.y + a.h + gap, w + 24.0, 0.0, 0.0, 1.0),
        "above" => (a.cx() - w / 2.0, a.y - gap - h, w + 24.0, 0.0, 0.0, -1.0),
        other => return Err(format!("side '{other}' is not right, left, above or below")),
    };
    let obstacles: Vec<Rect> = scene
        .live()
        .filter(|(_, el)| !is_bound_text(el) && !matches!(el_type(el), "arrow" | "frame" | "line"))
        .map(|(_, el)| rect_of(el))
        .collect();
    let free = |x: f64, y: f64| {
        let cand = Rect { x, y, w, h };
        !obstacles.iter().any(|o| cand.overlaps(o, 12.0))
    };
    // A scan along the side: 0, +1, -1, +2, -2 ... steps across it, then farther out.
    let (bx, by) = (x, y);
    'search: for out in 0..6 {
        for k in 0..=10i32 {
            let signed = if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) };
            let cx = bx + step_x * signed as f64 + push_x * 80.0 * out as f64;
            let cy = by + step_y * signed as f64 + push_y * 80.0 * out as f64;
            if free(cx, cy) {
                x = cx;
                y = cy;
                break 'search;
            }
        }
    }
    Ok((x, y))
}

/// An arrow's label sits on the middle of the arrow. When the two shapes are too close for it (the default
/// `near` gap is 60 px), push apart a shape made in this same call, along the line between their centres, until the
/// label clears both. Shapes already on the board are never moved: that would rewrite the person's board.
fn make_room_for_label(
    scene: &mut Scene,
    ctx: &mut Ctx,
    from: &str,
    to: &str,
    label: &str,
    movable: &BTreeSet<String>,
) {
    let (mover, anchor) = if movable.contains(to) {
        (to, from)
    } else if movable.contains(from) {
        (from, to)
    } else {
        return;
    };
    let (lw, lh) = measure(&wrap(label, 30));
    const PAD: f64 = 16.0;
    const STEP: f64 = 24.0;
    let mut moved = false;
    for _ in 0..80 {
        let (a, m) = (
            scene.elements[anchor].clone(),
            scene.elements[mover].clone(),
        );
        let (ca, cm) = (center_of(&a), center_of(&m));
        let (s, e) = (outline_point(&a, cm), outline_point(&m, ca));
        let mid = ((s.0 + e.0) / 2.0, (s.1 + e.1) / 2.0);
        let box_ = Rect {
            x: mid.0 - lw / 2.0,
            y: mid.1 - lh / 2.0,
            w: lw,
            h: lh,
        };
        if !box_.overlaps(&rect_of(&a), PAD) && !box_.overlaps(&rect_of(&m), PAD) {
            break;
        }
        let (mut dx, mut dy) = (cm.0 - ca.0, cm.1 - ca.1);
        let len = (dx * dx + dy * dy).sqrt();
        if len == 0.0 {
            (dx, dy) = (1.0, 0.0);
        } else {
            (dx, dy) = (dx / len, dy / len);
        }
        let (sx, sy) = (dx * STEP, dy * STEP);
        let text_id = bound_text(scene, &m).map(id_of);
        let shift = |el: &mut Element| {
            let (x, y) = (n(el, "x"), n(el, "y"));
            el.insert("x".into(), json!(x + sx));
            el.insert("y".into(), json!(y + sy));
        };
        touch(ctx, scene, mover, shift);
        if let Some(tid) = text_id {
            touch(ctx, scene, &tid, shift);
        }
        moved = true;
    }
    if moved {
        for arrow_id in arrows_bound_to(scene, mover) {
            reroute(ctx, scene, &arrow_id);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn connect(
    scene: &mut Scene,
    ctx: &mut Ctx,
    from: &str,
    to: &str,
    label: Option<&str>,
    style: Option<&str>,
    head: Option<&str>,
    movable: &BTreeSet<String>,
) -> String {
    if let Some(label) = label.filter(|l| !l.is_empty()) {
        make_room_for_label(scene, ctx, from, to, label, movable);
    }
    let from_el = scene.elements[from].clone();
    let to_el = scene.elements[to].clone();
    let start = outline_point(&from_el, center_of(&to_el));
    let end = outline_point(&to_el, center_of(&from_el));
    let (dx, dy) = (end.0 - start.0, end.1 - start.1);
    let mut arrow = base_element(ctx, scene, "arrow", start.0, start.1, dx.abs(), dy.abs());
    // Sharp, straight and thin, like the workflow canvas's crisp connectors.
    arrow.insert("roundness".into(), Value::Null);
    arrow.insert("points".into(), json!([[0, 0], [dx, dy]]));
    arrow.insert("lastCommittedPoint".into(), Value::Null);
    arrow.insert(
        "startBinding".into(),
        json!({ "elementId": from, "fixedPoint": [0.5, 0.5], "mode": "orbit" }),
    );
    arrow.insert(
        "endBinding".into(),
        json!({ "elementId": to, "fixedPoint": [0.5, 0.5], "mode": "orbit" }),
    );
    arrow.insert("startArrowhead".into(), Value::Null);
    arrow.insert(
        "endArrowhead".into(),
        if head == Some("none") {
            Value::Null
        } else {
            json!("arrow")
        },
    );
    arrow.insert("elbowed".into(), json!(false));
    if style == Some("dashed") {
        arrow.insert("strokeStyle".into(), json!("dashed"));
    }
    let arrow_id = insert_element(scene, arrow);
    for end_id in [from, to] {
        touch(ctx, scene, end_id, |e| add_bound(e, "arrow", &arrow_id));
    }
    if let Some(label) = label.filter(|l| !l.is_empty()) {
        let lines = wrap(label, 30);
        let (tw, th) = measure(&lines);
        let mid = ((start.0 + end.0) / 2.0, (start.1 + end.1) / 2.0);
        let t = text_element(
            ctx,
            scene,
            label,
            &lines,
            mid.0 - tw / 2.0,
            mid.1 - th / 2.0,
            Some(&arrow_id),
        );
        let tid = insert_element(scene, t);
        touch(ctx, scene, &arrow_id, |a| add_bound(a, "text", &tid));
    }
    arrow_id
}

fn make_frame(
    scene: &mut Scene,
    ctx: &mut Ctx,
    members: &[String],
    title: &str,
    created: &mut Vec<(String, String)>,
    refs: &mut Refs,
) {
    let mut all: BTreeSet<String> = BTreeSet::new();
    for id in members {
        all.insert(id.clone());
        all.extend(bound_ids(&scene.elements[id], "text"));
    }
    let mut acc: Option<(f64, f64, f64, f64)> = None;
    for id in members {
        let r = rect_of(&scene.elements[id]);
        acc = Some(match acc {
            None => (r.x, r.y, r.x + r.w, r.y + r.h),
            Some((a, b, c, d)) => (a.min(r.x), b.min(r.y), c.max(r.x + r.w), d.max(r.y + r.h)),
        });
    }
    let (x0, y0, x1, y1) = acc.expect("members is non-empty");
    let pad = 32.0;
    let mut frame = base_element(
        ctx,
        scene,
        "frame",
        x0 - pad,
        y0 - pad - 8.0,
        x1 - x0 + 2.0 * pad,
        y1 - y0 + 2.0 * pad + 8.0,
    );
    frame.insert("name".into(), json!(title));
    let frame_id = insert_element(scene, frame);
    for id in all {
        let fid = frame_id.clone();
        touch(ctx, scene, &id, |el| {
            el.insert("frameId".into(), json!(fid));
        });
    }
    refs.0.insert(frame_id.clone(), frame_id.clone());
    created.push((frame_id.clone(), frame_id));
}
