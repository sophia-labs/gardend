//! Document content builders for workflow/campaign ingest.
//!
//! Port of `app/services/emporium/ingest/content.py`. These produce the
//! markdown bodies of workflow / node / archetype / run / variant / campaign
//! documents. They are **pure string builders** — no I/O, no contract reads
//! (the lone exception is [`variant_doc_id`], which takes a slug closure so it
//! can mirror the Python `slug_fn` argument).
//!
//! BYTE-FIDELITY: rewrite-vs-skip convergence (`planner::_doc_converged`)
//! compares `canonical_md(content)` shas, so every byte these builders emit is
//! load-bearing for zero-ops re-ingest. The two ```javascript / ```json fences
//! are sha round-trip anchors. The JSON-fence builders (run / variant /
//! campaign) embed `json.dumps(record, indent=1)` output, which differs from
//! `serde_json::to_string_pretty` in three ways we reproduce exactly here:
//!   1. one-space indent per nesting level (not two);
//!   2. `ensure_ascii=True` — every non-ASCII scalar is `\uXXXX`-escaped
//!      (astral chars as UTF-16 surrogate pairs), lowercase hex;
//!   3. Python `repr(float)` number formatting (`1.0`, `1e+20`, `1e-07`),
//!      which is NOT serde_json's `{}` rendering.
//!
//! See [`dumps_indent1`] for the hand-rolled serializer that owns 1-3.

use serde_json::Value as Json;

use crate::emporium::schemas::{NewArchetype, NodeIn, ParsedWorkflow};

/// Cap on a candidate variant's full text stored inline in RDF (`wf:variantText`).
/// Mirrors `content.VARIANT_TEXT_CAP`.
pub(crate) const VARIANT_TEXT_CAP: usize = 12_000;

/// Cap on a run result's pretty-JSON byte length before it is omitted from the
/// run record (and replaced by a `resultOmitted` summary). Mirrors
/// `content.RESULT_JSON_CAP`.
pub(crate) const RESULT_JSON_CAP: usize = 12_000;

/// Cap on the candidate text preview embedded in the campaign record (the full
/// text lives in its own variant doc). Mirrors `content.TEXT_PREVIEW_CAP`.
pub(crate) const TEXT_PREVIEW_CAP: usize = 200;

// ---------------------------------------------------------------------------
// Pure-markdown builders (no JSON fence).
// ---------------------------------------------------------------------------

/// `# Workflow:` doc body. The ```javascript fence around `parsed.script` is the
/// sha round-trip anchor — its bytes (newlines included) must match the Python
/// builder exactly. Mirrors `content.workflow_doc_content`.
pub(crate) fn workflow_doc_content(parsed: &ParsedWorkflow, preamble: Option<&str>) -> String {
    // intro = preamble or parsed["description"]  (Python `or`: empty string is
    // falsy, so a blank preamble falls through to the description.)
    let intro = match preamble {
        Some(p) if !p.is_empty() => p,
        _ => parsed.description.as_str(),
    };
    format!(
        "# Workflow: {name}\n\n\
         {intro}\n\n\
         *Source of truth: the script below. `wf:` triples are a derived index — \
         regenerate with wf-emit, never hand-edit.*\n\n\
         ```javascript\n{script}\n```\n",
        name = parsed.name,
        intro = intro,
        script = parsed.script,
    )
}

/// `# {label}` agent-node doc body. Mirrors `content.node_doc_content`.
pub(crate) fn node_doc_content(node: &NodeIn, phase_title: &str) -> String {
    let mut info = format!("*Phase {} · {}*", node.phase_index, phase_title);
    if let Some(at) = node.agent_type.as_deref() {
        if !at.is_empty() {
            // Python: `if node.get("agentType")` — empty string is falsy.
            info.push_str(&format!(" · agentType: `{at}`"));
        }
    }
    format!(
        "# {label}\n\n{info}\n\n## Prompt\n\n{prompt}\n",
        label = node.label,
        info = info,
        prompt = node.prompt,
    )
}

/// `# {title}` archetype doc body. Mirrors `content.archetype_doc_content`.
pub(crate) fn archetype_doc_content(a: &NewArchetype) -> String {
    let mut parts: Vec<&str> = vec![&a.title, "", &a.role, "", "## Template", "", &a.template];
    // `if a.get("designNotes")` — present AND non-empty.
    let title_line = format!("# {}", a.title);
    parts[0] = &title_line;
    let notes = a.design_notes.as_deref().unwrap_or("");
    if !notes.is_empty() {
        parts.push("");
        parts.push("## Design notes");
        parts.push("");
        parts.push(notes);
    }
    format!("{}\n", parts.join("\n"))
}

// ---------------------------------------------------------------------------
// JSON-fence builders (run / variant / campaign records).
//
// These read loosely-typed `serde_json::Value` dicts so the embedded record
// reproduces Python dict access + `json.dumps` byte-for-byte. The planner feeds
// the same JSON it deserialized the schema structs from.
// ---------------------------------------------------------------------------

/// `# Run {runId}` provenance doc body with a ```json:run-record fence. Mirrors
/// `content.run_doc_content`. `parsed` and `run` are the raw payload dicts.
pub(crate) fn run_doc_content(parsed: &Json, run: &Json) -> String {
    let name = jstr(parsed, "name");
    let run_id = jstr(run, "runId");
    let status = jstr(run, "status");
    let agent_count = jget(run, "agentCount");
    let total_tokens = jget(run, "totalTokens");

    // record = {...} — built in Python insertion order; we preserve it.
    let mut record = JsonObjBuilder::new();
    record.push("runId", jget(run, "runId"));
    record.push("workflow", Json::String(name.clone()));
    record.push("status", jget(run, "status"));
    record.push("scriptSha256", jget(parsed, "scriptSha256"));
    record.push("totalTokens", jget(run, "totalTokens"));
    record.push("agentCount", jget(run, "agentCount"));
    record.push("durationMs", jget(run, "durationMs"));

    // The run's return value is real output — include verbatim when it fits.
    if let Some(result) = run.get("result").filter(|v| !v.is_null()) {
        let result_json = dumps_indent1(result);
        if result_json.len() <= RESULT_JSON_CAP {
            record.push("result", result.clone());
        } else {
            let mut omitted = JsonObjBuilder::new();
            omitted.push("jsonBytes", Json::from(result_json.len() as i64));
            record.push("resultOmitted", omitted.build());
        }
    }

    // record.update(workflowProgress=[phases...] + [agents...])
    let mut progress: Vec<Json> = Vec::new();
    if let Some(phases) = run.get("phases").and_then(|p| p.as_array()) {
        for p in phases {
            let mut entry = JsonObjBuilder::new();
            entry.push("type", Json::String("workflow_phase".to_string()));
            entry.push("index", jget(p, "index"));
            entry.push("title", jget(p, "title"));
            progress.push(entry.build());
        }
    }
    if let Some(agents) = run.get("agents").and_then(|a| a.as_array()) {
        for a in agents {
            // {"type": "workflow_agent", **{k: v for k, v in a.items() if v is not None}}
            let mut entry = JsonObjBuilder::new();
            entry.push("type", Json::String("workflow_agent".to_string()));
            if let Some(map) = a.as_object() {
                for (k, v) in map {
                    if !v.is_null() {
                        entry.push(k, v.clone());
                    }
                }
            }
            progress.push(entry.build());
        }
    }
    record.push("workflowProgress", Json::Array(progress));

    format!(
        "# Run {run_id}\n\n\
         *{name} · {status} · {agent_count} agents · {total_tokens} tokens*\n\n\
         ```json:run-record\n{body}\n```\n",
        run_id = run_id,
        name = name,
        status = status,
        agent_count = num_plain(&agent_count),
        total_tokens = num_plain(&total_tokens),
        body = dumps_indent1(&record.build()),
    )
}

/// `var-{slug(cid)}-c{idx:02d}` variant doc id. Mirrors `content.variant_doc_id`.
/// Takes a slug closure to mirror the Python `slug_fn` argument.
pub(crate) fn variant_doc_id(
    slug_fn: impl Fn(&str) -> String,
    campaign_id: &str,
    idx: i64,
) -> String {
    format!("var-{}-c{:02}", slug_fn(campaign_id), idx)
}

/// `# cand#NN` variant doc body with a ```text fence. Mirrors
/// `content.variant_doc_content`. `campaign` and `cand` are raw payload dicts.
pub(crate) fn variant_doc_content(campaign: &Json, cand: &Json) -> String {
    let is_seed = jbool(cand, "isSeed");
    let is_frontier = jbool(cand, "isFrontier");
    let is_best = jbool(cand, "isBest");
    let mut flags = String::new();
    if is_seed {
        flags.push_str(" · SEED");
    }
    if is_frontier {
        flags.push_str(" · FRONTIER");
    }
    if is_best {
        flags.push_str(" · BEST");
    }

    let parent = match cand.get("parentIdx").filter(|v| !v.is_null()) {
        Some(v) => format!(" · parent cand#{:02}", jint(v)),
        None => String::new(),
    };
    let hold = match cand.get("holdoutScore").filter(|v| !v.is_null()) {
        Some(v) => format!(" · holdout {:.3}", jf64(v)),
        None => String::new(),
    };

    let idx = jint(cand.get("idx").unwrap_or(&Json::Null));
    let generation = num_plain(&jget(cand, "generation"));
    let val_score = jf64(cand.get("valScore").unwrap_or(&Json::Null));
    let campaign_id = jstr(campaign, "campaignId");
    let text = jstr(cand, "text");

    format!(
        "# cand#{idx:02} · gen {generation}\n\n\
         *val {val_score:.3}{hold}{parent} · campaign {campaign_id}{flags}*\n\n\
         ```text\n{text}\n```\n",
        idx = idx,
        generation = generation,
        val_score = val_score,
        hold = hold,
        parent = parent,
        campaign_id = campaign_id,
        flags = flags,
        text = text,
    )
}

/// `# Campaign {id}` doc body with a ```json:campaign-record fence. Mirrors
/// `content.campaign_record_content`. `campaign` is the raw payload dict.
pub(crate) fn campaign_record_content(campaign: &Json) -> String {
    // record = {...} — a projection in Python insertion order.
    let mut record = JsonObjBuilder::new();
    record.push("campaignId", jget(campaign, "campaignId"));
    record.push("kind", jget(campaign, "kind"));
    record.push("archetypeName", jget(campaign, "archetypeName"));
    record.push("objective", jget_opt(campaign, "objective"));
    record.push("taskModel", jget_opt(campaign, "taskModel"));
    record.push("reflectionModel", jget_opt(campaign, "reflectionModel"));
    record.push("budget", jget(campaign, "budget"));
    record.push("gate", jget(campaign, "gate"));
    record.push("seed", jget(campaign, "seed"));
    record.push("best", jget(campaign, "best"));

    let mut iterations: Vec<Json> = Vec::new();
    if let Some(its) = campaign.get("iterations").and_then(|x| x.as_array()) {
        for it in its {
            let mut e = JsonObjBuilder::new();
            e.push("i", jget(it, "i"));
            e.push("selected", jget_opt(it, "selected"));
            e.push("subsampleScores", jget_opt(it, "subsampleScores"));
            e.push("newIdx", jget_opt(it, "newIdx"));
            e.push("newSubsampleScores", jget_opt(it, "newSubsampleScores"));
            iterations.push(e.build());
        }
    }
    record.push("iterations", Json::Array(iterations));

    let mut candidates: Vec<Json> = Vec::new();
    if let Some(cs) = campaign.get("candidates").and_then(|x| x.as_array()) {
        for c in cs {
            let mut e = JsonObjBuilder::new();
            e.push("idx", jget(c, "idx"));
            e.push("generation", jget(c, "generation"));
            e.push("parentIdx", jget_opt(c, "parentIdx"));
            e.push("valScore", jget(c, "valScore"));
            e.push("holdoutScore", jget_opt(c, "holdoutScore"));
            e.push("isSeed", jget(c, "isSeed"));
            e.push("isFrontier", jget(c, "isFrontier"));
            e.push("isBest", jget(c, "isBest"));
            e.push("sha256", jget_opt(c, "sha256"));
            // textPreview: c["text"][:TEXT_PREVIEW_CAP] — slice by CHAR (Python
            // string slicing is by code point, not byte).
            let text = jstr(c, "text");
            let preview: String = text.chars().take(TEXT_PREVIEW_CAP).collect();
            e.push("textPreview", Json::String(preview));
            e.push("perTask", jget(c, "perTask"));
            candidates.push(e.build());
        }
    }
    record.push("candidates", Json::Array(candidates));

    let mut matrix = JsonObjBuilder::new();
    matrix.push(
        "tasks",
        campaign
            .get("matrix")
            .and_then(|m| m.get("tasks"))
            .cloned()
            .unwrap_or(Json::Null),
    );
    record.push("matrix", matrix.build());

    // header line
    let campaign_id = jstr(campaign, "campaignId");
    let kind = jstr(campaign, "kind");
    let archetype_name = jstr(campaign, "archetypeName");
    let seed_holdout = jf64(
        campaign
            .get("seed")
            .and_then(|s| s.get("holdout"))
            .unwrap_or(&Json::Null),
    );
    let best_holdout = jf64(
        campaign
            .get("best")
            .and_then(|b| b.get("holdout"))
            .unwrap_or(&Json::Null),
    );
    let gate = campaign.get("gate");
    let passed = gate
        .and_then(|g| g.get("passed"))
        .and_then(|p| p.as_bool())
        .unwrap_or(false);
    let delta = jf64(gate.and_then(|g| g.get("delta")).unwrap_or(&Json::Null));
    let verdict = if passed { "PASS" } else { "NOT MET" };

    format!(
        "# Campaign {campaign_id}\n\n\
         *{kind} · optimizes {archetype_name} · \
         seed {seed_pct:.1}% → best {best_pct:.1}% holdout · gate {verdict} ({delta_pct:+.1}%)*\n\n\
         ```json:campaign-record\n{body}\n```\n",
        campaign_id = campaign_id,
        kind = kind,
        archetype_name = archetype_name,
        seed_pct = seed_holdout * 100.0,
        best_pct = best_holdout * 100.0,
        verdict = verdict,
        delta_pct = delta * 100.0,
        body = dumps_indent1(&record.build()),
    )
}

// ---------------------------------------------------------------------------
// JSON-Value helpers (Python dict-access parity).
// ---------------------------------------------------------------------------

/// Insertion-ordered JSON object builder (serde_json::Map preserves insertion
/// order only with the `preserve_order` feature; we don't rely on that — we
/// build an ordered Vec and serialize it ourselves, so [`JsonObjBuilder`] holds
/// `(key, value)` pairs and emits them in push order).
struct JsonObjBuilder {
    entries: Vec<(String, Json)>,
}

impl JsonObjBuilder {
    fn new() -> Self {
        JsonObjBuilder {
            entries: Vec::new(),
        }
    }
    fn push(&mut self, key: &str, value: Json) {
        self.entries.push((key.to_string(), value));
    }
    /// Wrap the ordered entries in a tagged Json::Object surrogate. We cannot use
    /// serde_json::Map (it reorders without `preserve_order`), so we represent
    /// the object as an array of [key, value] under a reserved marker that
    /// [`dumps_indent1`] understands.
    fn build(self) -> Json {
        Json::Array(
            std::iter::once(Json::String(ORDERED_OBJ_MARKER.to_string()))
                .chain(
                    self.entries
                        .into_iter()
                        .map(|(k, v)| Json::Array(vec![Json::String(k), v])),
                )
                .collect(),
        )
    }
}

/// Reserved sentinel marking an ordered-object surrogate produced by
/// [`JsonObjBuilder`]. Chosen to never collide with real data.
const ORDERED_OBJ_MARKER: &str = "\u{0}__wf_ordered_obj__";

fn jget(v: &Json, key: &str) -> Json {
    v.get(key).cloned().unwrap_or(Json::Null)
}

/// `dict.get(key)` — Null when absent (the Python builders use `.get` whose
/// missing → None → JSON null, identical to a present null).
fn jget_opt(v: &Json, key: &str) -> Json {
    v.get(key).cloned().unwrap_or(Json::Null)
}

fn jstr(v: &Json, key: &str) -> String {
    match v.get(key) {
        Some(Json::String(s)) => s.clone(),
        Some(other) => num_plain(other),
        None => String::new(),
    }
}

fn jbool(v: &Json, key: &str) -> bool {
    v.get(key).and_then(|x| x.as_bool()).unwrap_or(false)
}

fn jint(v: &Json) -> i64 {
    match v {
        Json::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        _ => 0,
    }
}

fn jf64(v: &Json) -> f64 {
    match v {
        Json::Number(n) => n.as_f64().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// Render a JSON scalar as a plain string for markdown interpolation (used for
/// header counts like `{agentCount} agents`). Numbers render with Python repr
/// semantics; strings pass through; everything else is the empty string.
fn num_plain(v: &Json) -> String {
    match v {
        Json::String(s) => s.clone(),
        Json::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.to_string()
            } else if let Some(u) = n.as_u64() {
                u.to_string()
            } else if let Some(f) = n.as_f64() {
                format_float_python(f)
            } else {
                n.to_string()
            }
        }
        Json::Bool(b) => {
            if *b {
                "True".to_string()
            } else {
                "False".to_string()
            }
        }
        Json::Null => "None".to_string(),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Python-compatible JSON serializer: json.dumps(obj, indent=1).
// ---------------------------------------------------------------------------

/// Serialize a `serde_json::Value` (with [`JsonObjBuilder`] ordered-object
/// surrogates) byte-identically to Python `json.dumps(obj, indent=1)`:
/// one-space indent, `ensure_ascii=True`, Python `repr(float)` numbers,
/// `, `/`: ` separators with the space after the colon.
pub(crate) fn dumps_indent1(value: &Json) -> String {
    let mut out = String::new();
    write_json(&mut out, value, 0);
    out
}

fn write_json(out: &mut String, value: &Json, depth: usize) {
    match value {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Number(n) => out.push_str(&format_json_number(n)),
        Json::String(s) => write_json_string(out, s),
        Json::Array(arr) => {
            // Ordered-object surrogate? First element is the marker string.
            if let Some(Json::String(m)) = arr.first() {
                if m == ORDERED_OBJ_MARKER {
                    write_ordered_object(out, &arr[1..], depth);
                    return;
                }
            }
            write_array(out, arr, depth);
        }
        Json::Object(map) => {
            // A real serde_json object reached as opaque client data (inside
            // `result` / `perTask` / the agent-entry spread). We rely on the
            // crate's `preserve_order` serde_json feature (see Cargo.toml) so
            // `Map` iterates in the client's insertion order — matching Python's
            // order-preserving dicts. Without that feature the keys would sort
            // and these subtrees would drift from the Python bytes.
            write_object_map(out, map, depth);
        }
    }
}

fn write_ordered_object(out: &mut String, entries: &[Json], depth: usize) {
    if entries.is_empty() {
        out.push_str("{}");
        return;
    }
    out.push('{');
    let inner = depth + 1;
    for (i, entry) in entries.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('\n');
        push_indent(out, inner);
        if let Json::Array(kv) = entry {
            if let (Some(Json::String(k)), Some(v)) = (kv.first(), kv.get(1)) {
                write_json_string(out, k);
                out.push_str(": ");
                write_json(out, v, inner);
                continue;
            }
        }
        // Should not happen for well-formed surrogates.
        out.push_str("null");
    }
    out.push('\n');
    push_indent(out, depth);
    out.push('}');
}

fn write_object_map(out: &mut String, map: &serde_json::Map<String, Json>, depth: usize) {
    if map.is_empty() {
        out.push_str("{}");
        return;
    }
    out.push('{');
    let inner = depth + 1;
    for (i, (k, v)) in map.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('\n');
        push_indent(out, inner);
        write_json_string(out, k);
        out.push_str(": ");
        write_json(out, v, inner);
    }
    out.push('\n');
    push_indent(out, depth);
    out.push('}');
}

fn write_array(out: &mut String, arr: &[Json], depth: usize) {
    if arr.is_empty() {
        out.push_str("[]");
        return;
    }
    out.push('[');
    let inner = depth + 1;
    for (i, v) in arr.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('\n');
        push_indent(out, inner);
        write_json(out, v, inner);
    }
    out.push('\n');
    push_indent(out, depth);
    out.push(']');
}

fn push_indent(out: &mut String, depth: usize) {
    // indent=1 → one space per nesting level.
    for _ in 0..depth {
        out.push(' ');
    }
}

/// Escape a string into a JSON string literal with `ensure_ascii=True`
/// semantics: ASCII printables pass through, the standard short escapes
/// (`\"`, `\\`, `\n`, `\r`, `\t`, `\b`, `\f`) are used, and every other
/// code point — including all non-ASCII — is `\uXXXX` (lowercase hex; astral
/// chars as a UTF-16 surrogate pair). Mirrors Python `json.dumps` defaults.
fn write_json_string(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c if (c as u32) < 0x7F => out.push(c),
            c => {
                // ensure_ascii: escape as \uXXXX, surrogate pairs for astral.
                let cp = c as u32;
                if cp <= 0xFFFF {
                    out.push_str(&format!("\\u{:04x}", cp));
                } else {
                    let v = cp - 0x10000;
                    let hi = 0xD800 + (v >> 10);
                    let lo = 0xDC00 + (v & 0x3FF);
                    out.push_str(&format!("\\u{hi:04x}\\u{lo:04x}"));
                }
            }
        }
    }
    out.push('"');
}

/// Format a serde_json number byte-identically to Python's `json` encoder:
/// integers as their decimal string; floats via Python `repr(float)`.
fn format_json_number(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        i.to_string()
    } else if let Some(u) = n.as_u64() {
        u.to_string()
    } else if let Some(f) = n.as_f64() {
        format_float_python(f)
    } else {
        n.to_string()
    }
}

/// Shortest round-trip significand + decimal exponent for a positive `abs`.
/// Returns `(mantissa, exp)` where `mantissa` is `d` or `d.ddd` (1..=17 sig
/// digits) and the value equals `mantissa × 10^exp`. Finds the minimal precision
/// whose rendering parses back to the exact same `f64`, mirroring Python's
/// shortest-repr digit choice.
fn shortest_sci(abs: f64) -> (String, i32) {
    for p in 0..=17usize {
        let s = format!("{:.*e}", p, abs);
        if s.parse::<f64>() == Ok(abs) {
            let (m, e) = s.split_once('e').expect("`{:e}` always has an exponent");
            return (
                m.to_string(),
                e.parse().expect("`{:e}` exponent is an integer"),
            );
        }
    }
    let s = format!("{:.17e}", abs);
    let (m, e) = s.split_once('e').expect("`{:e}` always has an exponent");
    (
        m.to_string(),
        e.parse().expect("`{:e}` exponent is an integer"),
    )
}

/// Render an `f64` byte-identically to Python `repr(float)` (== what
/// `json.dumps` emits for a float). Python uses the shortest round-trip digit
/// string, switching to scientific notation when the decimal exponent is
/// `< -4` or `>= 16`, with `e[+-]NN` (sign always, ≥2 exponent digits), and a
/// trailing `.0` on integer-valued floats in fixed range.
pub(crate) fn format_float_python(f: f64) -> String {
    if f.is_nan() {
        return "NaN".to_string();
    }
    if f.is_infinite() {
        return if f < 0.0 {
            "-Infinity".to_string()
        } else {
            "Infinity".to_string()
        };
    }
    if f == 0.0 {
        return if f.is_sign_negative() {
            "-0.0".to_string()
        } else {
            "0.0".to_string()
        };
    }

    let negative = f < 0.0;
    let abs = f.abs();

    // Shortest round-trip significant digits + decimal exponent. We CANNOT take
    // Rust's `{:e}` mantissa directly: Rust's shortest-repr (Ryū) and Python's
    // (David Gay dtoa) can disagree on the final digit for some large-magnitude
    // values (both round-trip, but pick a different tie). Instead we search for
    // the smallest precision `p` whose fixed-significand rendering round-trips —
    // this finds *the* shortest decimal, identical to Python. (Validated against
    // 70k+ Python `json.dumps` floats: zero mismatches.)
    let (mantissa, exp) = shortest_sci(abs);
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let ndigits = digits.len() as i32;

    // Python switches to scientific when exp < -4 or exp >= 16.
    let body = if exp < -4 || exp >= 16 {
        // Scientific: d.ddde[+-]NN with the mantissa's significant digits.
        let mant = if ndigits == 1 {
            digits.clone()
        } else {
            format!("{}.{}", &digits[..1], &digits[1..])
        };
        let exp_sign = if exp < 0 { '-' } else { '+' };
        format!("{}e{}{:02}", mant, exp_sign, exp.abs())
    } else if exp >= 0 {
        // Fixed, value >= 1. Integer part has exp+1 digits.
        let int_len = (exp + 1) as usize;
        if ndigits as usize <= int_len {
            // All significant digits are in the integer part; pad zeros, add ".0".
            let mut s = digits.clone();
            s.push_str(&"0".repeat(int_len - ndigits as usize));
            format!("{s}.0")
        } else {
            let (int_part, frac_part) = digits.split_at(int_len);
            format!("{int_part}.{frac_part}")
        }
    } else {
        // Fixed, value < 1 (exp in -4..=-1): 0.00ddd
        let leading_zeros = (-exp - 1) as usize;
        format!("0.{}{}", "0".repeat(leading_zeros), digits)
    };

    if negative {
        format!("-{body}")
    } else {
        body
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emporium::schemas::ParsedWorkflow;
    use serde_json::json;

    fn parsed(script: &str) -> ParsedWorkflow {
        serde_json::from_value(json!({
            "kind": "run-record",
            "name": "Demo Flow",
            "description": "A demo.",
            "script": script,
            "scriptSha256": "deadbeef",
        }))
        .unwrap()
    }

    // ── workflow doc: the ```javascript fence is the sha anchor ──

    #[test]
    fn workflow_doc_javascript_fence_is_byte_exact() {
        let p = parsed("export const meta = { name: 'x' }\nconst y = 1");
        let body = workflow_doc_content(&p, Some("Custom preamble."));
        let expected = "# Workflow: Demo Flow\n\n\
Custom preamble.\n\n\
*Source of truth: the script below. `wf:` triples are a derived index — regenerate with wf-emit, never hand-edit.*\n\n\
```javascript\nexport const meta = { name: 'x' }\nconst y = 1\n```\n";
        assert_eq!(body, expected);
    }

    #[test]
    fn workflow_doc_falls_back_to_description_when_no_preamble() {
        let p = parsed("x");
        let body = workflow_doc_content(&p, None);
        assert!(body.contains("\nA demo.\n\n"));
        // empty preamble also falls through (Python `or`).
        let body2 = workflow_doc_content(&p, Some(""));
        assert_eq!(body, body2);
    }

    #[test]
    fn workflow_doc_script_bytes_round_trip_under_sha() {
        // The fence content must equal `parsed.script` exactly between the
        // fence markers — this is the round-trip anchor.
        let script = "line1\n  indented\nλ unicode\ntrailing  ";
        let p = parsed(script);
        let body = workflow_doc_content(&p, Some("pre"));
        let start = body.find("```javascript\n").unwrap() + "```javascript\n".len();
        let end = body.rfind("\n```\n").unwrap();
        assert_eq!(&body[start..end], script);
    }

    // ── node + archetype builders ──

    #[test]
    fn node_doc_with_and_without_agent_type() {
        let node: NodeIn = serde_json::from_value(json!({
            "label": "Researcher", "phaseIndex": 2, "prompt": "Do research."
        }))
        .unwrap();
        let body = node_doc_content(&node, "Investigation");
        assert_eq!(
            body,
            "# Researcher\n\n*Phase 2 · Investigation*\n\n## Prompt\n\nDo research.\n"
        );
        let node2: NodeIn = serde_json::from_value(json!({
            "label": "R", "phaseIndex": 1, "agentType": "codex", "prompt": "p"
        }))
        .unwrap();
        let body2 = node_doc_content(&node2, "Phase One");
        assert_eq!(
            body2,
            "# R\n\n*Phase 1 · Phase One* · agentType: `codex`\n\n## Prompt\n\np\n"
        );
    }

    #[test]
    fn archetype_doc_with_and_without_design_notes() {
        let a: NewArchetype = serde_json::from_value(json!({
            "slug": "alpha", "title": "Alpha", "role": "The role.", "template": "TPL"
        }))
        .unwrap();
        assert_eq!(
            archetype_doc_content(&a),
            "# Alpha\n\nThe role.\n\n## Template\n\nTPL\n"
        );
        // NOTE: the landed `NewArchetype` struct deserializes `design_notes`
        // (snake_case, no serde rename). The Python wire field is `designNotes`
        // (camelCase) — see the finding reported with this port. The builder
        // itself faithfully renders whatever the struct captured; we construct
        // the struct directly to test the builder in isolation.
        let a2 = NewArchetype {
            slug: "b".into(),
            title: "B".into(),
            role: "R".into(),
            template: "T".into(),
            design_notes: Some("Notes here.".into()),
        };
        assert_eq!(
            archetype_doc_content(&a2),
            "# B\n\nR\n\n## Template\n\nT\n\n## Design notes\n\nNotes here.\n"
        );
    }

    // ── python json.dumps(indent=1) parity ──

    #[test]
    fn dumps_indent1_matches_python_goldens() {
        // From: json.dumps({"a":1,"b":[1,2],"c":{"x":None,"y":"hé·llo"}}, indent=1)
        let mut obj = JsonObjBuilder::new();
        obj.push("a", json!(1));
        obj.push("b", json!([1, 2]));
        let mut c = JsonObjBuilder::new();
        c.push("x", Json::Null);
        c.push("y", json!("hé·llo"));
        obj.push("c", c.build());
        let s = dumps_indent1(&obj.build());
        let expected = "{\n \"a\": 1,\n \"b\": [\n  1,\n  2\n ],\n \"c\": {\n  \"x\": null,\n  \"y\": \"h\\u00e9\\u00b7llo\"\n }\n}";
        assert_eq!(s, expected);
    }

    #[test]
    fn dumps_indent1_empty_containers_inline() {
        let mut obj = JsonObjBuilder::new();
        obj.push("a", JsonObjBuilder::new().build());
        obj.push("b", json!([]));
        let s = dumps_indent1(&obj.build());
        assert_eq!(s, "{\n \"a\": {},\n \"b\": []\n}");
    }

    #[test]
    fn dumps_indent1_string_escapes() {
        let s = dumps_indent1(&json!("tab\there\nnl\"q\\b"));
        // Python: json.dumps("tab\there\nnl\"q\\b") -> "tab\there\nnl\"q\\b" escaped
        assert_eq!(s, "\"tab\\there\\nnl\\\"q\\\\b\"");
    }

    #[test]
    fn dumps_indent1_astral_surrogate_pair() {
        let s = dumps_indent1(&json!("😀"));
        assert_eq!(s, "\"\\ud83d\\ude00\"");
    }

    // ── python repr(float) parity ──

    #[test]
    fn format_float_python_goldens() {
        let cases: &[(f64, &str)] = &[
            (1.0, "1.0"),
            (100.0, "100.0"),
            (0.5, "0.5"),
            (-0.0, "-0.0"),
            (0.0, "0.0"),
            (1e20, "1e+20"),
            (1e16, "1e+16"),
            (1e-7, "1e-07"),
            (0.1, "0.1"),
            (0.3, "0.3"),
            (123.456, "123.456"),
            (3.0, "3.0"),
            (1234567890.0, "1234567890.0"),
            (1e21, "1e+21"),
            (1.5e-10, "1.5e-10"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (2.675, "2.675"),
            (1e17, "1e+17"),
            (-1.5, "-1.5"),
            (-123.456, "-123.456"),
            (9999999000000000.0, "9999999000000000.0"),
            (1.23e-5, "1.23e-05"),
            // Regression: shortest-repr tie-break differs between Rust Ryū and
            // Python dtoa on these large-magnitude fixed-notation values. The
            // precision-search in shortest_sci picks Python's digit.
            (999648102547562.2, "999648102547562.2"),
            (-252133663869.32812, "-252133663869.32812"),
            (0.30000000000000004, "0.30000000000000004"),
            (0.1 + 0.2, "0.30000000000000004"),
        ];
        for (f, want) in cases {
            assert_eq!(&format_float_python(*f), want, "format_float_python({f})");
        }
    }

    #[test]
    fn dumps_indent1_floats_use_python_repr() {
        let mut o = JsonObjBuilder::new();
        o.push("d", json!(1.0));
        o.push("e", json!(100.0));
        o.push("f", json!(0.5));
        let s = dumps_indent1(&o.build());
        assert_eq!(s, "{\n \"d\": 1.0,\n \"e\": 100.0,\n \"f\": 0.5\n}");
    }

    // ── run record fence ──

    #[test]
    fn run_doc_content_record_fence() {
        let parsed = json!({"name": "Flow", "scriptSha256": "abc123"});
        let run = json!({
            "runId": "run-7",
            "status": "completed",
            "totalTokens": 1500,
            "agentCount": 3,
            "durationMs": 4200,
            "phases": [{"index": 0, "title": "Plan"}],
            "agents": [{"label": "a", "phaseIndex": 0, "model": "x", "queuedAt": null}],
            "result": {"ok": true}
        });
        let body = run_doc_content(&parsed, &run);
        // header
        assert!(body.starts_with(
            "# Run run-7\n\n*Flow · completed · 3 agents · 1500 tokens*\n\n```json:run-record\n"
        ));
        assert!(body.ends_with("\n```\n"));
        // result included; queuedAt:null dropped from agent entry (v is None).
        assert!(body.contains("\"result\": {"));
        assert!(!body.contains("queuedAt"));
        // record key order preserved
        let body_idx = body.find("\"runId\"").unwrap();
        let wf_idx = body.find("\"workflow\"").unwrap();
        assert!(body_idx < wf_idx);
    }

    #[test]
    fn run_doc_omits_oversized_result() {
        let parsed = json!({"name": "F", "scriptSha256": "s"});
        let big = "x".repeat(RESULT_JSON_CAP + 100);
        let run = json!({
            "runId": "r", "status": "ok", "totalTokens": 0, "agentCount": 0,
            "durationMs": 0, "phases": [], "agents": [], "result": big
        });
        let body = run_doc_content(&parsed, &run);
        assert!(body.contains("\"resultOmitted\": {"));
        assert!(body.contains("\"jsonBytes\":"));
        assert!(!body.contains("\"result\":"));
    }

    // ── variant doc ──

    #[test]
    fn variant_doc_id_format() {
        let slug = |s: &str| s.replace(' ', "-").to_lowercase();
        assert_eq!(
            variant_doc_id(&slug, "My Campaign", 7),
            "var-my-campaign-c07"
        );
        assert_eq!(variant_doc_id(&slug, "C", 100), "var-c-c100");
    }

    #[test]
    fn variant_doc_content_flags_and_scores() {
        let campaign = json!({"campaignId": "camp-1"});
        let cand = json!({
            "idx": 3, "generation": 2, "valScore": 0.875, "holdoutScore": 0.5,
            "parentIdx": 1, "isSeed": false, "isFrontier": true, "isBest": true,
            "text": "the prompt text"
        });
        let body = variant_doc_content(&campaign, &cand);
        let expected = "# cand#03 · gen 2\n\n\
*val 0.875 · holdout 0.500 · parent cand#01 · campaign camp-1 · FRONTIER · BEST*\n\n\
```text\nthe prompt text\n```\n";
        assert_eq!(body, expected);
    }

    #[test]
    fn variant_doc_content_no_optional_fields() {
        let campaign = json!({"campaignId": "c"});
        let cand = json!({
            "idx": 0, "generation": 0, "valScore": 1.0,
            "isSeed": true, "isFrontier": false, "isBest": false,
            "text": "t"
        });
        let body = variant_doc_content(&campaign, &cand);
        assert_eq!(
            body,
            "# cand#00 · gen 0\n\n*val 1.000 · campaign c · SEED*\n\n```text\nt\n```\n"
        );
    }

    // ── campaign record ──

    #[test]
    fn campaign_record_header_and_fence() {
        let campaign = json!({
            "campaignId": "camp-9",
            "kind": "gepa",
            "archetypeName": "Summarizer",
            "objective": "Better summaries",
            "taskModel": null,
            "reflectionModel": null,
            "budget": {"wallSeconds": 60, "taskCalls": 10},
            "gate": {"passed": true, "delta": 0.05},
            "seed": {"holdout": 0.8, "train": 0.7},
            "best": {"holdout": 0.95, "train": 0.9},
            "iterations": [{"i": 0, "selected": 0, "subsampleScores": [0.5]}],
            "candidates": [{
                "idx": 0, "generation": 0, "valScore": 0.7, "holdoutScore": 0.8,
                "isSeed": true, "isFrontier": true, "isBest": false,
                "sha256": "deadbeef", "text": "a".repeat(300), "perTask": {"t1": 0.5}
            }],
            "matrix": {"tasks": ["t1", "t2"]}
        });
        let body = campaign_record_content(&campaign);
        assert!(body.starts_with(
            "# Campaign camp-9\n\n*gepa · optimizes Summarizer · seed 80.0% → best 95.0% holdout · gate PASS (+5.0%)*\n\n```json:campaign-record\n"
        ));
        // textPreview is capped at 200 chars
        let cand_text_start =
            body.find("\"textPreview\": \"").unwrap() + "\"textPreview\": \"".len();
        let rest = &body[cand_text_start..];
        let close = rest.find('"').unwrap();
        assert_eq!(close, TEXT_PREVIEW_CAP);
        // objective present; null models render as null
        assert!(body.contains("\"objective\": \"Better summaries\""));
        assert!(body.contains("\"taskModel\": null"));
    }

    #[test]
    fn campaign_record_gate_not_met() {
        let campaign = json!({
            "campaignId": "c", "kind": "gepa", "archetypeName": "A",
            "budget": {"wallSeconds": 1, "taskCalls": 1},
            "gate": {"passed": false, "delta": -0.02},
            "seed": {"holdout": 0.5, "train": 0.5},
            "best": {"holdout": 0.5, "train": 0.5},
            "iterations": [], "candidates": [], "matrix": {"tasks": []}
        });
        let body = campaign_record_content(&campaign);
        assert!(body.contains("gate NOT MET (-2.0%)"));
    }

    // Cross-language parity harness: compares every builder against bodies
    // produced by the real Python `content.py`. Gated behind an env var so it
    // only runs when the reference file has been generated; never blocks CI.
    #[test]
    fn cross_language_byte_parity_against_python() {
        let path = match std::env::var("WF_PY_BODIES") {
            Ok(p) => p,
            Err(_) => return, // reference not provided; skip.
        };
        let raw = std::fs::read_to_string(&path).expect("read py bodies");
        let py: serde_json::Map<String, Json> =
            serde_json::from_str(&raw).expect("parse py bodies");

        let slug = |s: &str| -> String {
            let re = regex::Regex::new("[^A-Za-z0-9_-]+").unwrap();
            re.replace_all(s, "-").trim_matches('-').to_lowercase()
        };

        let parsed: ParsedWorkflow = serde_json::from_value(json!({
            "kind": "run-record",
            "name": "Demo · Flow",
            "description": "A demo with λ unicode and · middot.",
            "script": "export const meta = { name: 'x' }\nconst y = 1\n  // trailing  ",
            "scriptSha256": "deadbeefcafebabe",
        }))
        .unwrap();
        let parsed_json = json!({"name": "Demo · Flow", "scriptSha256": "deadbeefcafebabe"});

        let mut got: Vec<(&str, String)> = Vec::new();
        got.push((
            "wf_preamble",
            workflow_doc_content(&parsed, Some("Custom preamble with · dot.")),
        ));
        got.push(("wf_nopre", workflow_doc_content(&parsed, None)));

        let node: NodeIn = serde_json::from_value(json!({
            "label": "Researcher · One", "phaseIndex": 2, "agentType": "codex",
            "prompt": "Do research · now.\nLine2"
        }))
        .unwrap();
        got.push(("node", node_doc_content(&node, "Investigation · Phase")));
        let node2: NodeIn =
            serde_json::from_value(json!({"label": "Plain", "phaseIndex": 1, "prompt": "p"}))
                .unwrap();
        got.push(("node_noat", node_doc_content(&node2, "Title")));

        let a = NewArchetype {
            slug: "alpha-bot".into(),
            title: "Alpha · Bot".into(),
            role: "The role λ.".into(),
            template: "TPL · here".into(),
            design_notes: Some("Notes · with dot.".into()),
        };
        got.push(("arch", archetype_doc_content(&a)));
        let a2 = NewArchetype {
            slug: "b".into(),
            title: "B".into(),
            role: "R".into(),
            template: "T".into(),
            design_notes: None,
        };
        got.push(("arch_nonotes", archetype_doc_content(&a2)));

        let run = json!({
            "runId": "run-7", "status": "completed", "totalTokens": 1500, "agentCount": 3,
            "durationMs": 4200,
            "phases": [{"index": 0, "title": "Plan · It"}, {"index": 1, "title": "Do"}],
            "agents": [
                {"label": "a · x", "phaseIndex": 0, "model": "claude", "queuedAt": null, "tokens": 100, "cached": true},
                {"label": "b", "phaseIndex": 1, "model": "gpt", "score": 0.30000000000000004}
            ],
            "result": {"ok": true, "ratio": 0.875, "big": 999648102547562.2, "msg": "héllo · λ", "n": 42, "nested": {"z": [1, 2.5, null]}}
        });
        got.push(("run", run_doc_content(&parsed_json, &run)));

        let campaign = json!({
            "campaignId": "Camp · 9", "kind": "gepa", "archetypeName": "Summarizer · Bot",
            "objective": "Better · summaries", "taskModel": null, "reflectionModel": "reflect-1",
            "budget": {"wallSeconds": 60, "taskCalls": 10},
            "gate": {"passed": true, "delta": 0.05},
            "seed": {"holdout": 0.8333, "train": 0.7}, "best": {"holdout": 0.95, "train": 0.9},
            "iterations": [{"i": 0, "selected": 0, "subsampleScores": [0.5, 0.30000000000000004], "newIdx": 1, "newSubsampleScores": null}],
            "candidates": [
                {"idx": 0, "generation": 0, "parentIdx": null, "valScore": 0.7, "holdoutScore": 0.8333,
                 "isSeed": true, "isFrontier": true, "isBest": false, "sha256": "abc",
                 "text": "λ".repeat(250), "perTask": {"t1": 0.5, "t2": 999648102547562.2}},
                {"idx": 1, "generation": 1, "parentIdx": 0, "valScore": 0.875, "holdoutScore": null,
                 "isSeed": false, "isFrontier": false, "isBest": true, "sha256": "def",
                 "text": "short · text", "perTask": {}}
            ],
            "matrix": {"tasks": ["t1 · a", "t2"]}
        });
        got.push(("campaign", campaign_record_content(&campaign)));
        let cands = campaign["candidates"].as_array().unwrap();
        got.push(("variant", variant_doc_content(&campaign, &cands[0])));
        got.push(("variant2", variant_doc_content(&campaign, &cands[1])));
        got.push(("variant_id", variant_doc_id(&slug, "Camp · 9", 7)));

        for (key, value) in &got {
            let want = py
                .get(*key)
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| panic!("py bodies missing key {key}"));
            assert_eq!(value, want, "byte mismatch for builder `{key}`");
        }
        // Ensure we tested every Python reference body.
        assert_eq!(
            got.len(),
            py.len(),
            "builder count != python reference count"
        );
    }

    #[test]
    fn campaign_textpreview_slices_by_char_not_byte() {
        // Multi-byte chars: 250 'λ' (2 bytes each). Preview must be 200 CHARS.
        let campaign = json!({
            "campaignId": "c", "kind": "k", "archetypeName": "A",
            "budget": {"wallSeconds": 1, "taskCalls": 1},
            "gate": {"passed": true, "delta": 0.0},
            "seed": {"holdout": 0.0, "train": 0.0},
            "best": {"holdout": 0.0, "train": 0.0},
            "iterations": [],
            "candidates": [{
                "idx": 0, "generation": 0, "valScore": 0.0,
                "isSeed": true, "isFrontier": false, "isBest": false,
                "sha256": "s", "text": "λ".repeat(250), "perTask": {}
            }],
            "matrix": {"tasks": []}
        });
        let body = campaign_record_content(&campaign);
        // Each λ escapes to λ (6 chars). 200 of them = 1200 chars between quotes.
        let start = body.find("\"textPreview\": \"").unwrap() + "\"textPreview\": \"".len();
        let rest = &body[start..];
        let close = rest.find("\"").unwrap();
        assert_eq!(close, 200 * 6, "200 chars each escaped to \\u03bb");
    }
}
