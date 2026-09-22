//! import.webClip — URL/HTML clip import for headless cells.
//! Port of frontend/src/native/native-local-runtime.ts importWebClip plus its
//! pure helpers (parseLocalWebClip, extractWebClipTitle/Author,
//! cleanWebClipElement, webClipNode(Children)ToMarkdown, absoluteWebClipUrl,
//! normalizeWebClipText/Markdown).
//!
//! Pipeline notes (mirrors the TS exactly):
//! - The loopback route (src/loopback_web_import_routes.rs) has already
//!   fetched the URL (validate_http_url + fetch_limited_url_bytes) and put
//!   the HTML in the payload, so this handler never touches the network.
//!   `documentId` is pre-injected by crdt_queue normalize_payload_ids.
//! - The TS does NOT run Readability here: it extracts a content root via
//!   WEB_CLIP_CONTENT_SELECTORS, strips noise tags/classes, and converts the
//!   DOM to MARKDOWN, then writes the document with format "markdown" (the
//!   already-ported content_parse markdown path) — not via html_to_tiptap.
//! - Document creation composes a `document.write` CrdtOperation through
//!   super::document_ops::document_write, like import_vault_ops does. The TS
//!   importWebClip has NO completion-ledger replay guard (unlike
//!   import.vault / graph.importArchive), so none is added here.
//!
//! Known fidelity gaps:
//! - JS `String.prototype.trim` vs the markdown normalizers use the JS \s
//!   set; mirrored via is_js_whitespace.
//! - `new URL(url).host` errors surface as "Invalid URL" (Node's TypeError
//!   message) — only reachable when og:title/<title>/<h1> are all missing
//!   AND the URL is unparsable (the route always validates first).

use crate::app_runtime::AppHandle;
use crate::crdt_engine::executor::{ApplyOperationError, ApplyOperationResult};
use crate::crdt_queue::CrdtOperation;
use serde_json::{json, Map as JsonMap, Value};
use std::sync::LazyLock;

use super::content_parse::html_to_tiptap::{
    convert_element, is_js_whitespace, DomElement, DomNode,
};
use super::import_vault_ops::{create_import_result, import_result_envelope, numeric_timestamp};

// ─────────────────────────────────────────────────────────────────────────────
// Constants (native-local-runtime.ts)
// ─────────────────────────────────────────────────────────────────────────────

const WEB_CLIP_REMOVE_TAGS: &[&str] = &[
    "script", "style", "noscript", "iframe", "svg", "canvas", "nav", "footer", "header", "aside",
    "form", "button", "input", "select", "textarea",
];

const WEB_CLIP_NOISE_WORDS: &[&str] = &[
    "sidebar",
    "menu",
    "nav",
    "navigation",
    "footer",
    "header",
    "comment",
    "comments",
    "ad",
    "advertisement",
    "social",
    "share",
    "sharing",
    "related",
    "recommended",
    "signup",
    "newsletter",
    "popup",
    "modal",
    "cookie",
    "banner",
];

const WEB_CLIP_CONTENT_SELECTORS: &[&str] = &[
    "article",
    "[role=\"main\"]",
    "main",
    ".post-content",
    ".article-content",
    ".entry-content",
    ".content",
    "#content",
    ".post",
    ".article",
];

static CONTENT_SELECTORS: LazyLock<Vec<scraper::Selector>> = LazyLock::new(|| {
    WEB_CLIP_CONTENT_SELECTORS
        .iter()
        .map(|s| scraper::Selector::parse(s).expect("valid selector"))
        .collect()
});

fn selector(s: &str) -> scraper::Selector {
    scraper::Selector::parse(s).expect("valid selector")
}

// ─────────────────────────────────────────────────────────────────────────────
// JS string helpers
// ─────────────────────────────────────────────────────────────────────────────

fn trim_js(text: &str) -> &str {
    text.trim_matches(is_js_whitespace)
}

/// normalizeWebClipText: `.replace(/\s+/gu, ' ').trim()`
fn normalize_web_clip_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut in_ws = false;
    for c in value.chars() {
        if is_js_whitespace(c) {
            if !in_ws {
                out.push(' ');
                in_ws = true;
            }
        } else {
            out.push(c);
            in_ws = false;
        }
    }
    trim_js(&out).to_string()
}

/// normalizeWebClipMarkdown: strip line-trailing spaces/tabs, collapse 3+
/// newlines to 2, trim.
fn normalize_web_clip_markdown(value: &str) -> String {
    let mut stripped = String::with_capacity(value.len());
    for (index, line) in value.split('\n').enumerate() {
        if index > 0 {
            stripped.push('\n');
        }
        stripped.push_str(line.trim_end_matches([' ', '\t']));
    }
    trim_js(&collapse_blank_lines(&stripped)).to_string()
}

/// `.replace(/\n{3,}/gu, '\n\n')`
fn collapse_blank_lines(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut newlines = 0usize;
    for c in value.chars() {
        if c == '\n' {
            newlines += 1;
            if newlines <= 2 {
                out.push('\n');
            }
        } else {
            newlines = 0;
            out.push(c);
        }
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// parseLocalWebClip
// ─────────────────────────────────────────────────────────────────────────────

pub struct ParsedWebClip {
    pub title: String,
    pub markdown: String,
    pub warnings: Vec<String>,
}

/// Port of parseLocalWebClip(html, url, titleOverride).
pub fn parse_local_web_clip(
    html: &str,
    url: &str,
    title_override: Option<&str>,
) -> Result<ParsedWebClip, String> {
    let document = scraper::Html::parse_document(html);
    let title = match title_override
        .map(trim_js)
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .or_else(|| extract_web_clip_title(&document))
    {
        Some(title) => title,
        // new URL(url).host — TypeError('Invalid URL') on bad input.
        None => {
            let parsed = url::Url::parse(url).map_err(|_| "Invalid URL".to_string())?;
            let host = parsed.host_str().unwrap_or_default().to_string();
            match parsed.port() {
                Some(port) => format!("{host}:{port}"),
                None => host,
            }
        }
    };
    let author = extract_web_clip_author(&document);

    let content_root: DomElement = CONTENT_SELECTORS
        .iter()
        .find_map(|sel| document.select(sel).next())
        .or_else(|| document.select(&selector("body")).next())
        .map(convert_element)
        .unwrap_or_else(|| convert_element(document.root_element()));

    let body_markdown = web_clip_children_to_markdown(&content_root, url);
    let body_markdown = trim_js(&body_markdown);
    let mut meta = vec![format!("Source: {url}")];
    if let Some(author) = author {
        meta.push(format!("Author: {author}"));
    }
    let assembled = [
        format!("# {title}"),
        String::new(),
        meta.join(" | "),
        String::new(),
        "---".to_string(),
        String::new(),
        body_markdown.to_string(),
    ]
    .join("\n");
    let markdown = trim_js(&collapse_blank_lines(&assembled)).to_string();
    Ok(ParsedWebClip {
        title,
        markdown,
        warnings: vec![
            "Local web clip used the desktop HTML extractor and Markdown write path.".to_string(),
        ],
    })
}

fn extract_web_clip_title(document: &scraper::Html) -> Option<String> {
    let og_title = document
        .select(&selector("meta[property=\"og:title\"]"))
        .next()
        .and_then(|el| el.value().attr("content"))
        .map(trim_js)
        .filter(|t| !t.is_empty());
    if let Some(title) = og_title {
        return Some(title.to_string());
    }
    let title_text = document
        .select(&selector("title"))
        .next()
        .map(|el| el.text().collect::<String>())
        .map(|t| trim_js(&t).to_string())
        .filter(|t| !t.is_empty());
    if let Some(title) = title_text {
        for separator in [" | ", " - ", " — ", " – ", " :: "] {
            if title.contains(separator) {
                let head = title.split(separator).next().map(trim_js).unwrap_or("");
                return Some(if head.is_empty() {
                    title.clone()
                } else {
                    head.to_string()
                });
            }
        }
        return Some(title);
    }
    document
        .select(&selector("h1"))
        .next()
        .map(|el| el.text().collect::<String>())
        .map(|t| trim_js(&t).to_string())
        .filter(|t| !t.is_empty())
}

fn extract_web_clip_author(document: &scraper::Html) -> Option<String> {
    for sel in [
        "meta[name=\"author\"]",
        "meta[property=\"author\"]",
        "meta[name=\"article:author\"]",
        "meta[property=\"article:author\"]",
    ] {
        let value = document
            .select(&selector(sel))
            .next()
            .and_then(|el| el.value().attr("content"))
            .map(trim_js)
            .filter(|v| !v.is_empty());
        if let Some(value) = value {
            return Some(value.to_string());
        }
    }
    None
}

// ─────────────────────────────────────────────────────────────────────────────
// cleanWebClipElement (as a logical skip predicate — removal and attribute
// stripping never affect the markdown output beyond subtree exclusion)
// ─────────────────────────────────────────────────────────────────────────────

fn is_web_clip_removed(el: &DomElement) -> bool {
    WEB_CLIP_REMOVE_TAGS.contains(&el.tag.as_str()) || is_web_clip_noise_element(el)
}

/// Port of isWebClipNoiseElement: lowercase(class words + id), [-_]+ → space,
/// any word in the noise set.
fn is_web_clip_noise_element(el: &DomElement) -> bool {
    let classes = el
        .attr("class")
        .map(|value| value.split_ascii_whitespace().collect::<Vec<_>>().join(" "))
        .unwrap_or_default();
    let id = el.attr("id").unwrap_or_default();
    let combined = format!("{classes} {id}").to_lowercase();
    let combined: String = combined
        .chars()
        .map(|c| if c == '-' || c == '_' { ' ' } else { c })
        .collect();
    combined
        .split(is_js_whitespace)
        .any(|word| WEB_CLIP_NOISE_WORDS.contains(&word))
}

/// DOM textContent over the cleaned tree (removed subtrees excluded).
fn clean_text_content(el: &DomElement, out: &mut String) {
    for child in &el.children {
        match child {
            DomNode::Text(text) => out.push_str(text),
            DomNode::Element(child_el) => {
                if !is_web_clip_removed(child_el) {
                    clean_text_content(child_el, out);
                }
            }
        }
    }
}

fn text_content(el: &DomElement) -> String {
    let mut out = String::new();
    clean_text_content(el, &mut out);
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// webClipNodeToMarkdown and friends
// ─────────────────────────────────────────────────────────────────────────────

fn web_clip_children_to_markdown(el: &DomElement, source_url: &str) -> String {
    let lines: Vec<String> = el
        .children
        .iter()
        .map(|child| web_clip_node_to_markdown(child, source_url))
        .collect();
    normalize_web_clip_markdown(&lines.join("\n"))
}

fn web_clip_node_to_markdown(node: &DomNode, source_url: &str) -> String {
    let el = match node {
        DomNode::Text(text) => return normalize_web_clip_text(text),
        DomNode::Element(el) => el,
    };
    if is_web_clip_removed(el) {
        return String::new();
    }
    let tag = el.tag.as_str();
    if let Some(level) = heading_level(tag) {
        let text = normalize_web_clip_text(&text_content(el));
        return if text.is_empty() {
            String::new()
        } else {
            format!("\n{} {}\n", "#".repeat(level), text)
        };
    }
    match tag {
        "p" => {
            let text = web_clip_inline_children_to_markdown(el, source_url);
            let text = trim_js(&text);
            if text.is_empty() {
                String::new()
            } else {
                format!("\n{text}\n")
            }
        }
        "ul" | "ol" => {
            let items: Vec<String> = el
                .children
                .iter()
                .filter_map(|child| match child {
                    DomNode::Element(li) if li.tag == "li" && !is_web_clip_removed(li) => Some(li),
                    _ => None,
                })
                .enumerate()
                .map(|(index, li)| {
                    let text = web_clip_inline_children_to_markdown(li, source_url);
                    let text = trim_js(&text);
                    if text.is_empty() {
                        String::new()
                    } else if tag == "ol" {
                        format!("{}. {}", index + 1, text)
                    } else {
                        format!("- {text}")
                    }
                })
                .filter(|line| !line.is_empty())
                .collect();
            format!("\n{}\n", items.join("\n"))
        }
        "blockquote" => {
            let text = normalize_web_clip_markdown(&web_clip_children_to_markdown(el, source_url));
            if text.is_empty() {
                String::new()
            } else {
                let quoted: Vec<String> =
                    text.split('\n').map(|line| format!("> {line}")).collect();
                format!("\n{}\n", quoted.join("\n"))
            }
        }
        "pre" => {
            let text = text_content(el);
            if trim_js(&text).is_empty() {
                String::new()
            } else {
                format!("\n```\n{}\n```\n", text.trim_end_matches('\n'))
            }
        }
        "hr" => "\n---\n".to_string(),
        "img" => image_markdown(el, source_url),
        _ => web_clip_children_to_markdown(el, source_url),
    }
}

fn heading_level(tag: &str) -> Option<usize> {
    match tag {
        "h1" => Some(1),
        "h2" => Some(2),
        "h3" => Some(3),
        "h4" => Some(4),
        "h5" => Some(5),
        "h6" => Some(6),
        _ => None,
    }
}

fn image_markdown(el: &DomElement, source_url: &str) -> String {
    let src = absolute_web_clip_url(el.attr("src").unwrap_or(""), source_url);
    let alt = normalize_web_clip_text(el.attr("alt").unwrap_or(""));
    if src.is_empty() {
        String::new()
    } else {
        format!("![{alt}]({src})")
    }
}

fn web_clip_inline_children_to_markdown(el: &DomElement, source_url: &str) -> String {
    let joined: String = el
        .children
        .iter()
        .map(|child| web_clip_inline_node_to_markdown(child, source_url))
        .collect();
    normalize_web_clip_text(&joined)
}

fn web_clip_inline_node_to_markdown(node: &DomNode, source_url: &str) -> String {
    let el = match node {
        DomNode::Text(text) => return text.clone(),
        DomNode::Element(el) => el,
    };
    if is_web_clip_removed(el) {
        return String::new();
    }
    let inline_text = |el: &DomElement| -> String {
        el.children
            .iter()
            .map(|child| web_clip_inline_node_to_markdown(child, source_url))
            .collect()
    };
    match el.tag.as_str() {
        "a" => {
            let label = normalize_web_clip_text(&inline_text(el));
            let href = absolute_web_clip_url(el.attr("href").unwrap_or(""), source_url);
            if !href.is_empty() && !label.is_empty() {
                format!("[{label}]({href})")
            } else {
                label
            }
        }
        "strong" | "b" => format!("**{}**", normalize_web_clip_text(&inline_text(el))),
        "em" | "i" => format!("*{}*", normalize_web_clip_text(&inline_text(el))),
        "code" => format!("`{}`", normalize_web_clip_text(&text_content(el))),
        "s" | "strike" | "del" => format!("~~{}~~", normalize_web_clip_text(&inline_text(el))),
        "br" => "\n".to_string(),
        "img" => image_markdown(el, source_url),
        _ => inline_text(el),
    }
}

/// Port of absoluteWebClipUrl: new URL(value, sourceUrl).toString(), falling
/// back to the raw value.
fn absolute_web_clip_url(value: &str, source_url: &str) -> String {
    if trim_js(value).is_empty() {
        return String::new();
    }
    match url::Url::parse(source_url).and_then(|base| base.join(value)) {
        Ok(resolved) => resolved.to_string(),
        Err(_) => value.to_string(),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Payload helpers (JS ?? / stringValue semantics, mirroring import_vault_ops)
// ─────────────────────────────────────────────────────────────────────────────

fn object_payload(value: &Value) -> JsonMap<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

/// First key present with a non-null value (a JS `a ?? b` chain).
fn pick<'a>(map: &'a JsonMap<String, Value>, keys: &[&str]) -> Option<&'a Value> {
    for key in keys {
        if let Some(value) = map.get(*key) {
            if !value.is_null() {
                return Some(value);
            }
        }
    }
    None
}

/// Port of stringValue/nullableStringValue: '' and null → None.
fn string_value(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::Null => None,
        Value::String(text) if text.is_empty() => None,
        Value::String(text) => Some(text.clone()),
        Value::Bool(flag) => Some(flag.to_string()),
        Value::Number(number) => Some(match number.as_i64() {
            Some(i) => i.to_string(),
            None => number.to_string(),
        }),
        _ => None,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Handler (importWebClip)
// ─────────────────────────────────────────────────────────────────────────────

pub(crate) async fn apply(
    app: &AppHandle,
    operation: &CrdtOperation,
) -> Result<serde_json::Value, String> {
    apply_classified(app, operation)
        .await
        .map_err(ApplyOperationError::into_message)
}

pub(crate) async fn apply_classified(
    app: &AppHandle,
    operation: &CrdtOperation,
) -> ApplyOperationResult<serde_json::Value> {
    let value = object_payload(&operation.payload);
    let document_id = string_value(pick(&value, &["documentId", "document_id"]))
        .ok_or_else(|| "import.webClip: documentId is required".to_string())?;
    let url = string_value(pick(&value, &["url"]))
        .ok_or_else(|| "import.webClip: url is required".to_string())?;
    let html = string_value(pick(&value, &["html"]))
        .ok_or_else(|| "import.webClip: html is required".to_string())?;
    let title_override = string_value(pick(&value, &["title"]));

    let mut import_result = create_import_result();
    let parsed = parse_local_web_clip(&html, &url, title_override.as_deref())?;
    import_result.warnings.extend(parsed.warnings);
    if trim_js(&parsed.markdown).is_empty() {
        import_result.status = "error";
        import_result
            .errors
            .push("Could not extract readable content from the page".to_string());
        return Ok(import_result_envelope(&import_result));
    }

    // this.writeDocument(documentId, {...}, operationId, enqueueTimestamp):
    // composed as a document.write through the ported handler. `order` falls
    // back to the enqueue timestamp (the inline payload supplies none).
    let write_operation = CrdtOperation {
        operation_id: operation.operation_id.clone(),
        kind: "document.write".to_string(),
        graph_id: operation.graph_id.clone(),
        document_id: Some(document_id.clone()),
        payload: json!({
            "documentId": document_id,
            "title": parsed.title,
            "content": parsed.markdown,
            "format": "markdown",
            "parentId": string_value(pick(&value, &["folderId", "folder_id"])),
            "order": numeric_timestamp(&operation.enqueue_timestamp),
            "readOnly": false,
        }),
        enqueue_timestamp: operation.enqueue_timestamp.clone(),
    };
    super::document_ops::document_write_classified(app, &write_operation).await?;
    import_result.documents_created = 1;
    import_result.document_ids.push(document_id);
    Ok(import_result_envelope(&import_result))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = r#"<!DOCTYPE html>
<html><head>
<title>Garden Notes | Some Site</title>
<meta property="og:title" content="Garden Notes">
<meta name="author" content="A. Writer">
</head><body>
<nav class="navigation">skip me</nav>
<article>
  <h1>Growing Things</h1>
  <p>Plants need <strong>light</strong> and <a href="/water">water</a>.</p>
  <div class="social-share">share buttons</div>
  <ul><li>one</li><li>two</li></ul>
  <ol><li>first</li><li>second</li></ol>
  <blockquote><p>Be patient.</p></blockquote>
  <pre>code here
</pre>
  <hr>
  <img src="/pic.png" alt="A pic">
</article>
</body></html>"#;

    #[test]
    fn parses_clip_to_markdown() {
        let parsed = parse_local_web_clip(PAGE, "https://example.com/post?x=1", None).unwrap();
        assert_eq!(parsed.title, "Garden Notes");
        let md = &parsed.markdown;
        assert!(md.starts_with("# Garden Notes\n\nSource: https://example.com/post?x=1 | Author: A. Writer\n\n---\n"), "got: {md}");
        assert!(md.contains("# Growing Things"));
        assert!(md.contains("Plants need **light** and [water](https://example.com/water)."));
        assert!(!md.contains("share buttons"));
        assert!(!md.contains("skip me"));
        assert!(md.contains("- one\n- two"));
        assert!(md.contains("1. first\n2. second"));
        assert!(md.contains("> Be patient."));
        assert!(md.contains("```\ncode here\n```"));
        assert!(md.contains("![A pic](https://example.com/pic.png)"));
        assert!(md.contains("\n---\n"));
        assert_eq!(parsed.warnings.len(), 1);
    }

    #[test]
    fn title_fallbacks() {
        // og:title absent → <title> with separator split
        let html =
            "<html><head><title>Post Title - Site</title></head><body><p>x</p></body></html>";
        let parsed = parse_local_web_clip(html, "https://example.com/", None).unwrap();
        assert_eq!(parsed.title, "Post Title");
        // override wins
        let parsed =
            parse_local_web_clip(html, "https://example.com/", Some("  Custom  ")).unwrap();
        assert_eq!(parsed.title, "Custom");
        // nothing → URL host
        let parsed = parse_local_web_clip(
            "<html><body><p>x</p></body></html>",
            "https://sub.example.com:8443/a",
            None,
        )
        .unwrap();
        assert_eq!(parsed.title, "sub.example.com:8443");
    }

    /// Regression pin for the 2026-06-11 "worm news" incident: a repro driven
    /// through POST /graphs/:id/import/clip appeared to show this parser
    /// dropping og:title and the article body. The actual cause was the route
    /// itself (loopback_web_import_routes.rs): ClipUrlRequest has no `html`
    /// field, so the caller-supplied HTML was silently ignored and the LIVE
    /// https://wormnews.net/pool-pattern page (og:title "worm news", empty
    /// client-rendered <worm-news-app> body) was fetched and parsed instead.
    /// This test locks in that the parser itself handles the repro input
    /// correctly: og:title wins over the em-dash <title>, and the article
    /// body (heading, bold paragraph, list items) survives.
    #[test]
    fn repro_pool_pattern() {
        let html = r#"<html><head><title>Pool Pattern — Worm News</title><meta property="og:title" content="The Pool Pattern"/></head><body><article><h1>The Pool Pattern</h1><p>Cells own <b>state</b>; pure functions go to pools.</p><ul><li>embeddings</li><li>parsing</li></ul></article></body></html>"#;
        let parsed = parse_local_web_clip(html, "https://wormnews.net/pool-pattern", None).unwrap();
        assert_eq!(parsed.title, "The Pool Pattern");
        assert!(parsed.markdown.contains("# The Pool Pattern\n"));
        assert!(parsed.markdown.contains("**state**"));
        assert!(parsed.markdown.contains("- embeddings\n- parsing"));
    }

    #[test]
    fn empty_content_yields_empty_body() {
        let parsed = parse_local_web_clip(
            "<html><body><nav>only nav</nav></body></html>",
            "https://example.com/",
            Some("T"),
        )
        .unwrap();
        // body markdown empty → markdown is just the header block
        assert!(parsed.markdown.ends_with("---"), "got: {}", parsed.markdown);
    }
}
