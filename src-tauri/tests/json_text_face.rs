//! JSON authored as text reads back verbatim through the Markdown face.
//!
//! Garden documents hold graph-resident contracts as JSON *text*: Phanes'
//! control bundle, channel catalog, ingestion policy, privacy epoch binding
//! and operations status (canonical one-line JSON written with
//! `format: "plain"`, reread byte for byte), and the definitions, grants,
//! consents, prompt bindings and tool manifests that Choreograph
//! `JSON.parse`s from `read_document(format: "markdown")`. Before the
//! block-contract engine that face was the block text, verbatim. The new
//! exporter (src/crdt_engine/markdown_export.rs, garden 3b1c9ea / c60f057)
//! backslash-escapes `*`, backticks, non-intraword `_`, `\` before
//! punctuation, `<tag`, `~~`, `[^` and `]:` in running text, so since the
//! 2026-10-04 cell roll those documents read back as invalid JSON (`\_` and
//! `` \` `` are not JSON escapes) or as bytes that differ from the write.
//!
//! The rule under test (markdown_export.rs `authored_json_text`): a document
//! whose blocks are all paragraphs of text carrying no mark the Markdown face
//! can express (plus hard breaks), and whose text taken whole parses as one
//! JSON object or array, reads back as that text, verbatim. It is decided
//! from the stored document alone, so it covers every document already
//! written, by any writer, without rewriting one.
//!
//! These legs are pure: `write_document`'s parser plus the block contract's
//! normaliser (what `document.write` stores, crdt_engine/document_ops.rs),
//! then the exporter. The store leg, through the real MCP handlers and the
//! HTTP export route, is the lib test `json_text_face_store_tests`.

#[path = "../src/crdt_engine/content_parse.rs"]
mod content_parse;
#[path = "../src/crdt_engine/markdown_export.rs"]
mod markdown_export;

use serde_json::{json, Value};

/// Phanes' control-bundle shape (phanes/bot/control/store.py
/// `_write_unlocked`): canonical one-line JSON, sorted keys, no trailing
/// newline, written with `format: "plain"` and reread byte for byte. Strings
/// carry `**bold**`, inline backticks, non-intraword `_`, and the JSON
/// escapes `\n`, `\"` and `\\`, plus every other character the exporter
/// escapes in running text (`<tag`, `~~`, `[^`, `]:`, `*`).
const CONTROL_BUNDLE: &str = r##"{"agent":{"kind":"agent","objectId":"urn:sophia:agent:agent-ded0c28b107012ad"},"object":{"kind":"agent-interaction-control-bundle","version":"sha256:c5a9b8c1"},"presentation":{"templates":[{"key":"privacy.saved","surface":"ephemeral","text":"Saved **{label}**. Use `/phanes privacy` to change it."},{"key":"attachment-suffix","surface":"attachment-suffix","text":"\n\n_…full response attached._"},{"key":"privacy.status","surface":"embed-body","text":"Current preference: **{status}**\n\n{transition}"},{"key":"quote","surface":"ephemeral","text":"She said \"hi\" \\ <b>bold</b> ~~old~~ [^1] a|b ]: x * y"}]},"schema":"sophia.agent-interaction-control-bundle.v1"}"##;

/// The prompt-binding shape (phanes/bot/phanes_prime_seed.py, Choreograph
/// src/workflow/agent-prompt-document.ts): the rendered prompt rides inside
/// a JSON string with backticks, `discord__search_messages`, `_emphasis_`,
/// quotes, a backslash and `**`.
fn prompt_binding() -> Value {
    json!({
        "agentId": "agent-ded0c28b107012ad",
        "bindingId": "apb_4ff7385bc7a532250fa9aed2",
        "inference": {"model": "google/gemini-2.5-flash", "provider": "openrouter"},
        "promptDocumentId": "phanes-prime-prompt-v1",
        "registeredName": "Phanes",
        "schema": "sophia.agent-prompt-binding.v1",
        "snapshot": {
            "blocks": [],
            "documentId": "phanes-prime-prompt-v1",
            "renderedDigest": "510adc10f0636022f5a8bbdeb693a7f77ff6c0b65e586f2c739686a800bff0e6",
            "renderedText": "You are Phanes.\n\nUse `discord__search_messages`, `discord__message_context`, and exact reads.\n\nReturn a `sophia.proposed-effect.v1`; quote the user \"exactly\" and _never_ guess a C:\\path or a **secret**.",
            "schema": "sophia.agent-prompt-snapshot.v1"
        },
        "status": "active"
    })
}

/// The tool-manifest shape (phanes/bot/phanes_prime_seed.py
/// `TOOL_MANIFEST_DOCUMENT_ID`): input-schema patterns with `_[` and `9_-`,
/// a description with `*emphasis*`, a `discord__` tool name. It carries no
/// Markdown hint (no backtick pair, `**`, `__x__`, link or line-start
/// marker), so a write without `format` detects it as plain.
fn tool_manifest() -> Value {
    json!({
        "agentId": "agent-ded0c28b107012ad",
        "capabilities": [{
            "access": "write",
            "approvalPolicy": {"mode": "on-risk"},
            "backend": "garden-mcp",
            "capabilityId": "cap_mnemosyne",
            "tools": [{
                "description": "Search the *bounded* privacy projection; at most 50 rows, newest first.",
                "inputSchema": {
                    "properties": {
                        "handle": {"pattern": "^[a-z]+_[A-Za-z0-9_-]{24}$", "type": "string"},
                        "query": {"type": "string"}
                    },
                    "required": ["handle", "query"],
                    "type": "object"
                },
                "name": "discord__search_messages"
            }]
        }],
        "schema": "sophia.agent-tool-manifest.v1"
    })
}

/// The Phanes seed's JSON document bytes: 2-space indented, trailing newline
/// (`json.dumps(value, indent=2, sort_keys=True) + "\n"`).
fn pretty(value: &Value) -> String {
    format!("{}\n", serde_json::to_string_pretty(value).expect("serialize"))
}

const SEED: &str = "op-json-text-face";

/// What `write_document(content, format)` stores: the parser, then the
/// block contract's normaliser.
fn store(content: &str, format: Option<&str>) -> Value {
    let parsed = content_parse::parse_write_content_for_operation(content, format, SEED)
        .expect("parse write content");
    let nodes = parsed.tiptap_json["content"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let (nodes, _receipts) =
        content_parse::block_contract::normalise_doc_content_at(&nodes, SEED, "content");
    json!({"type": "doc", "content": nodes})
}

fn detected_format(content: &str) -> String {
    content_parse::parse_write_content_for_operation(content, None, SEED)
        .expect("parse write content")
        .source_format
}

/// `read_document(format: "markdown")` of a stored document.
fn face(doc: &Value) -> String {
    markdown_export::tiptap_json_to_markdown(doc)
}

/// The text of a stored document that is a single paragraph.
fn single_paragraph_text(doc: &Value) -> String {
    let blocks = doc["content"].as_array().expect("doc content");
    assert_eq!(blocks.len(), 1, "expected one stored paragraph: {doc}");
    assert_eq!(blocks[0]["type"], "paragraph", "{doc}");
    blocks[0]["content"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|node| {
            assert!(node.get("marks").is_none(), "unexpected mark: {node}");
            node["text"].as_str().unwrap_or("")
        })
        .collect()
}

fn parse_json(text: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("not JSON ({e}): {text}"))
}

fn text(value: &str) -> Value {
    json!({"type": "text", "text": value})
}

fn paragraph(content: Vec<Value>) -> Value {
    json!({"type": "paragraph", "content": content})
}

fn doc(blocks: Vec<Value>) -> Value {
    json!({"type": "doc", "content": blocks})
}

// ---------------------------------------------------------------------------
// 1. Phanes' control bundle: one-line canonical JSON, byte-equal.
// ---------------------------------------------------------------------------

#[test]
fn control_bundle_canonical_json_reads_back_byte_equal() {
    let stored = store(CONTROL_BUNDLE, Some("plain"));
    // The write keeps the authored text: one unmarked paragraph.
    assert_eq!(single_paragraph_text(&stored), CONTROL_BUNDLE);
    let read = face(&stored);
    assert_eq!(read, CONTROL_BUNDLE, "the markdown face escaped JSON text");
    assert_eq!(parse_json(&read), parse_json(CONTROL_BUNDLE));
}

// ---------------------------------------------------------------------------
// 2. Pretty-printed multi-line JSON (the Phanes seed's bytes).
// ---------------------------------------------------------------------------

/// `format: "plain"` stores pretty-printed JSON as ONE paragraph: the plain
/// parser (content_parse.rs `text_to_tiptap_json`, a port of the TS
/// `textToTipTapJson`, unchanged since 2026-06-11) folds newlines into
/// spaces and collapses runs of spaces. No face can return the indented
/// bytes, so the contract is: the face is the stored text, byte for byte,
/// and it is JSON-equal to what was written.
#[test]
fn pretty_printed_prompt_binding_and_tool_manifest_read_back_as_stored() {
    for (name, value) in [("prompt binding", prompt_binding()), ("tool manifest", tool_manifest())] {
        let source = pretty(&value);
        let stored = store(&source, Some("plain"));
        let stored_text = single_paragraph_text(&stored);
        let read = face(&stored);
        assert_eq!(read, stored_text, "{name}: the markdown face is not the stored text");
        assert_eq!(parse_json(&read), value, "{name}: not JSON-equal to the write");
    }
}

#[test]
fn json_written_without_a_format_that_detects_as_plain_reads_back_as_stored() {
    let value = tool_manifest();
    let source = pretty(&value);
    assert_eq!(detected_format(&source), "plain");
    let stored = store(&source, None);
    let read = face(&stored);
    assert_eq!(read, single_paragraph_text(&stored));
    assert_eq!(parse_json(&read), value);
}

/// Shapes another writer can leave (the app editor, an import): one
/// paragraph per line, or lines joined by hard breaks. Taken whole —
/// paragraphs joined by a blank line, a hard break as a newline — the text
/// is one JSON object, so it reads back verbatim.
#[test]
fn json_split_across_paragraphs_or_hard_breaks_reads_back_verbatim() {
    let lines = [
        "{",
        r#"  "pattern": "^[a-z]+_[A-Za-z0-9_-]{24}$","#,
        r#"  "text": "Saved **{label}**. Use `/phanes privacy`.""#,
        "}",
    ];
    let per_line = doc(lines.iter().map(|line| paragraph(vec![text(line)])).collect());
    let read = face(&per_line);
    assert_eq!(read, lines.join("\n\n"));
    parse_json(&read);

    let mut inlines = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            inlines.push(json!({"type": "hardBreak"}));
        }
        inlines.push(text(line));
    }
    let hard_breaks = doc(vec![paragraph(inlines)]);
    let read = face(&hard_breaks);
    assert_eq!(read, lines.join("\n"));
    parse_json(&read);

    // Empty paragraphs (the editor's trailing one) are skipped, as the
    // Markdown face skips them.
    let trailing = doc(vec![
        paragraph(vec![text(CONTROL_BUNDLE)]),
        json!({"type": "paragraph"}),
    ]);
    assert_eq!(face(&trailing), CONTROL_BUNDLE);
}

/// Marks with no Markdown form (comment anchors, highlights, text style —
/// the block contract's "text only" marks) never reach the Markdown face,
/// so they do not stop JSON from reading back verbatim: commenting on a
/// config document in the app must not break the agent that reads it.
#[test]
fn text_only_marks_do_not_hide_json() {
    let commented = doc(vec![paragraph(vec![
        text(r#"{"a":"_x"#),
        json!({"type": "text", "text": "*y*",
               "marks": [{"type": "commentMark", "attrs": {"commentId": "c-1"}},
                         {"type": "highlight"}]}),
        text(r#""}"#),
    ])]);
    assert_eq!(face(&commented), r#"{"a":"_x*y*"}"#);
}

// ---------------------------------------------------------------------------
// 3. Prose keeps its escaping.
// ---------------------------------------------------------------------------

/// Guard (passes before and after the fix): text that is not, taken whole,
/// a JSON object or array keeps every escape the Markdown face needs.
#[test]
fn plain_prose_keeps_its_escaping() {
    let cases = [
        (
            "Plain prose keeps its escaping: *stars*, _under_ and `ticks`, but snake_case stays.",
            r"Plain prose keeps its escaping: \*stars\*, \_under\_ and \`ticks\`, but snake_case stays.",
        ),
        // Opens like JSON, is not JSON.
        ("{draft} with *stars*", r"{draft} with \*stars\*"),
        // A JSON scalar is not a contract document.
        (r#""a *quoted* string""#, r#""a \*quoted\* string""#),
        // Invalid JSON (trailing comma).
        (r#"[1, 2, "*"],"#, r#"[1, 2, "\*"],"#),
    ];
    for (authored, expected) in cases {
        assert_eq!(face(&store(authored, Some("plain"))), expected, "{authored}");
    }

    // Two JSON values in two paragraphs are not ONE JSON value.
    let two = store("{\"a\":\"*\"}\n\n{\"b\":\"_\"}", Some("plain"));
    assert_eq!(face(&two), "{\"a\":\"\\*\"}\n\n{\"b\":\"\\_\"}");

    // A JSON paragraph under a heading is a Markdown document, not JSON text.
    let titled = doc(vec![
        json!({"type": "heading", "attrs": {"level": 1}, "content": [text("Config")]}),
        paragraph(vec![text(r#"{"a":"*"}"#)]),
    ]);
    assert_eq!(face(&titled), "# Config\n\n{\"a\":\"\\*\"}");
}

// ---------------------------------------------------------------------------
// 4. Marked text that looks like JSON.
// ---------------------------------------------------------------------------

/// Guard (passes before and after the fix). A paragraph carrying code or
/// bold marks keeps the Markdown face even when its text looks like JSON:
/// the marks are content the author made (a Markdown write, the editor),
/// emitting the bare text would drop them silently, and the face must stay
/// the inverse of the Markdown parser (re-importing it gives the same
/// document). The escape outside the code span (`\_`) is therefore kept.
#[test]
fn marked_text_that_looks_like_json_keeps_the_markdown_face() {
    let marked = doc(vec![paragraph(vec![
        text(r#"{"path":"_drafts/"#),
        json!({"type": "text", "text": "x", "marks": [{"type": "code"}]}),
        text(r#"", "note":""#),
        json!({"type": "text", "text": "y", "marks": [{"type": "bold"}]}),
        text(r#""}"#),
    ])]);
    let read = face(&marked);
    assert_eq!(read, r#"{"path":"\_drafts/`x`", "note":"**y**"}"#);
    assert_eq!(face(&store(&read, Some("markdown"))), read, "the face re-imports to itself");
}

/// Characterization (passes before and after the fix): JSON written WITHOUT
/// a format that carries a Markdown hint (here a backtick pair) is parsed as
/// Markdown at write time. The parser consumes the backslash of `\"` and
/// turns the span into a code mark, so the stored text is no longer the
/// JSON that was sent; no read face can restore it. Writers of JSON must
/// say `format: "plain"` (or store a fenced `json` code block).
#[test]
fn json_detected_as_markdown_is_changed_at_write_not_at_read() {
    let authored = r#"{"q":"say \"hi\" with `x`"}"#;
    assert_eq!(detected_format(authored), "markdown");
    let read = face(&store(authored, None));
    assert_eq!(read, r#"{"q":"say "hi" with `x`"}"#);
    assert!(serde_json::from_str::<Value>(&read).is_err());
}

/// The one cost of the rule, pinned: a Markdown author who backslash-escapes
/// delimiters inside a paragraph that is, taken whole, a JSON object reads
/// it back unescaped (the JSON reading wins), so for that paragraph
/// Markdown → store → Markdown → store is not a fixed point. Plain-written
/// JSON is the case the face exists for; nobody escapes Markdown inside a
/// JSON object on purpose.
#[test]
fn markdown_escaped_delimiters_inside_a_json_paragraph_read_back_as_json() {
    let stored = store(r#"{"a": "\*\*b\*\*"}"#, Some("markdown"));
    assert_eq!(single_paragraph_text(&stored), r#"{"a": "**b**"}"#);
    assert_eq!(face(&stored), r#"{"a": "**b**"}"#);
}

// ---------------------------------------------------------------------------
// The fenced form (Phanes' control bundle since 2026-10-05 23:00Z; the
// Phanes seed's JSON documents since 2026-08-03; the app's control editor).
// ---------------------------------------------------------------------------

/// Characterization (passes before and after the fix): a JSON document
/// stored as a fenced `json` code block is a `codeBlock`, not a paragraph,
/// so the rule never applies to it; the code block arm emits the code text
/// verbatim inside the fence. Phanes (`_json_document`) and Choreograph
/// (`unwrapAuthoredJsonDocument`) strip the fence.
#[test]
fn fenced_json_code_block_reads_back_verbatim() {
    // The containment form: canonical one-line JSON in a fence.
    let fenced = format!("```json\n{}\n```", CONTROL_BUNDLE);
    let stored = store(&format!("{}\n", fenced), Some("markdown"));
    assert_eq!(stored["content"][0]["type"], "codeBlock", "{stored}");
    assert_eq!(face(&stored), fenced);

    // The Phanes seed form (bot/domain_seed.py `_document_write_payload`):
    // pretty-printed JSON in a fence keeps its indentation.
    let value = prompt_binding();
    let source = pretty(&value);
    let stored = store(&format!("```json\n{}\n```\n", source.trim()), Some("markdown"));
    let read = face(&stored);
    assert_eq!(read, format!("```json\n{}\n```", source.trim()));
    let inner = read
        .strip_prefix("```json\n")
        .and_then(|rest| rest.strip_suffix("\n```"))
        .expect("three-backtick json fence");
    assert_eq!(parse_json(inner), value);
}

/// Characterization (passes before and after the fix), a hazard for the
/// consumers: the exporter grows the fence past the longest backtick run
/// inside the code, so fenced JSON whose strings hold a ``` run reads back
/// with a four-backtick fence, which Phanes' and Choreograph's unwrappers
/// (exactly three backticks) do not strip.
#[test]
fn fenced_json_holding_a_triple_backtick_reads_back_with_a_longer_fence() {
    let json_text = r#"{"t":"see ```py fence```"}"#;
    let stored = store(&format!("```json\n{}\n```\n", json_text), Some("markdown"));
    assert_eq!(face(&stored), format!("````json\n{}\n````", json_text));
}

// ---------------------------------------------------------------------------
// 5. Round trips.
// ---------------------------------------------------------------------------

/// write(plain) → read(markdown) is byte-equal for canonical JSON, and the
/// face is a fixed point: writing it back as plain text stores the same
/// text and reads back the same bytes.
#[test]
fn json_face_is_a_fixed_point_of_plain_rewrite() {
    let once = face(&store(CONTROL_BUNDLE, Some("plain")));
    assert_eq!(once, CONTROL_BUNDLE);
    let twice = face(&store(&once, Some("plain")));
    assert_eq!(twice, once);

    for value in [prompt_binding(), tool_manifest()] {
        let once = face(&store(&pretty(&value), Some("plain")));
        let rewritten = store(&once, Some("plain"));
        assert_eq!(single_paragraph_text(&rewritten), once);
        assert_eq!(face(&rewritten), once);
        assert_eq!(parse_json(&once), value);
    }
}
