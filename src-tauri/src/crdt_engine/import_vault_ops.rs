//! import.vault — Obsidian/Notion/Roam archive import for headless cells.
//!
//! Port of:
//! - frontend/src/native/native-local-runtime.ts importVaultArchive (~2224)
//!   plus its pure helpers (createArchiveImportFolders,
//!   prepareArchiveTagHubDocuments, archiveFolderIdForDocument,
//!   resolveArchiveTitle, importResultEnvelope, fnv1aPath, ...);
//! - frontend/src/native/archive-import.ts (all of it: parseObsidianZip,
//!   parseNotionZip, parseRoamZip, wikilink/tag extraction,
//!   archiveTitleLookupKeys, folder detection, skip rules).
//!
//! Layering (mirrors block_ops/workspace_ops):
//! - a pure layer (zip parsing + title resolution + import planning) with no
//!   app dependencies, exercised standalone by tests/import_vault_ops.rs via
//!   the #[path] shim pattern;
//! - an impure `apply` handler that executes the plan through the already
//!   ported `document_ops::document_write` and `workspace_ops::apply`
//!   (workspace.createFolder / workspace.createWire), with the Tier B
//!   completion-ledger replay guard and pending-upload handling.
//!
//! Known fidelity gaps (documented, not load-bearing for the parity zips):
//! - `archiveTitleLookupKeys` calls String.prototype.normalize('NFKC'); we
//!   ship no Unicode tables, so `nfkc_approx` below covers the compatibility
//!   mappings that occur in real vault titles (fullwidth ASCII, NBSP-family
//!   spaces, Latin ligatures) but not canonical (de)composition.
//! - JS `\s` and Rust regex `\s` differ on U+0085/U+FEFF; FEFF is stripped by
//!   the invisible-character pass first, so this is unobservable in practice.
//! - `Array.prototype.sort()` is UTF-16 code-unit order; Rust string `Ord` is
//!   byte order. They diverge only between U+E000..U+FFFF and astral chars.
//! - `localeCompare` (folder creation order) is approximated with a
//!   case-insensitive-primary comparison.
//! - `numericTimestamp`'s `Date.parse` fallback is not ported (no date
//!   parser in the crate); enqueue timestamps are epoch-ms strings here.

use super::executor::{ApplyOperationError, ApplyOperationResult};
use crate::app_runtime::AppHandle;
use crate::crdt_queue::CrdtOperation;
use regex::Regex;
use serde_json::{json, Map as JsonMap, Value};
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

// ─────────────────────────────────────────────────────────────────────────────
// Constants (archive-import.ts)
// ─────────────────────────────────────────────────────────────────────────────

const MAX_EXTRACTED_SIZE: usize = 500 * 1024 * 1024;
const MAX_FILE_COUNT: usize = 10_000;
const MAX_PAGE_COUNT: usize = 10_000;
const OBSIDIAN_SKIP_FILES: [&str; 5] = [".DS_Store", "Thumbs.db", ".obsidian", ".trash", ".logseq"];
const OBSIDIAN_SKIP_EXTENSIONS: [&str; 3] = [".canvas", ".json", ".css"];
const NOTION_SKIP_EXTENSIONS: [&str; 7] =
    [".json", ".png", ".jpg", ".jpeg", ".gif", ".svg", ".webp"];
const SYSTEM_ROAM_PAGES: [&str; 4] = ["TODO", "DONE", "embed", "query"];

const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

// ─────────────────────────────────────────────────────────────────────────────
// Pure types (archive-import.ts interfaces)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArchiveImportSource {
    Obsidian,
    Notion,
    Roam,
}

#[derive(Debug, Clone)]
pub(crate) struct ArchiveImportedDocument {
    pub(crate) title: String,
    pub(crate) markdown: String,
    pub(crate) folder_path: Option<String>,
    pub(crate) source_filename: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct ArchiveWikiLink {
    pub(crate) source_title: String,
    pub(crate) target_title: String,
    pub(crate) display_text: Option<String>,
    pub(crate) source_context: Option<String>,
    pub(crate) is_tag: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ArchiveImportParseResult {
    pub(crate) documents: Vec<ArchiveImportedDocument>,
    pub(crate) wikilinks: Vec<ArchiveWikiLink>,
    pub(crate) warnings: Vec<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Regexes (compiled once; raw patterns mirror archive-import.ts)
// ─────────────────────────────────────────────────────────────────────────────

fn regex(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("static regex"))
}

fn image_embed_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    regex(&RE, r"!\[\[([^\]]+)\]\]")
}

fn wikilink_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    regex(&RE, r"\[\[([^\]]+)\]\]")
}

fn code_block_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    regex(&RE, r"(?s)```.*?```|`[^`]+`")
}

fn merge_emoji_wikilink_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    regex(
        &RE,
        r"\[\[([^\]\n]*?)\]\]\s+([A-Z][a-zA-Z]*(?:\s+[A-Z][a-zA-Z]*)*)",
    )
}

fn notion_uuid_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    regex(&RE, r"(?i)\s+[0-9a-f]{32}$")
}

fn notion_link_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    // JS: /\[([^\]]*)\]\((?!https?:\/\/)([^)]+\.md)\)/giu — the regex crate has
    // no lookahead, so capture any href and apply the https?:// + .md rules in
    // extract_notion_links (equivalent match set; see tests).
    regex(&RE, r"\[([^\]]*)\]\(([^)]+)\)")
}

fn todo_marker_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    regex(&RE, r"\{\{\[\[TODO\]\]\}\}\s*")
}

fn done_marker_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    regex(&RE, r"\{\{\[\[DONE\]\]\}\}\s*")
}

fn block_ref_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    regex(&RE, r"\(\(([a-zA-Z0-9_-]{9,})\)\)")
}

fn embed_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    regex(
        &RE,
        r"(?i)\{\{(?:\[\[embed\]\]:\s*|embed:\s*)\(\(([^)]+)\)\)\}\}",
    )
}

fn attribute_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    regex(&RE, r"(?m)^([A-Za-z][A-Za-z0-9 _-]*)::(.*)$")
}

fn roam_date_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    regex(
        &RE,
        r"^(January|February|March|April|May|June|July|August|September|October|November|December)\s+(\d{1,2})(st|nd|rd|th),\s+(\d{4})$",
    )
}

fn iso_date_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    regex(&RE, r"^(\d{4})[-/_](\d{2})[-/_](\d{2})$")
}

// ─────────────────────────────────────────────────────────────────────────────
// JS string semantics helpers
// ─────────────────────────────────────────────────────────────────────────────

/// JS `\s` (WhiteSpace ∪ LineTerminator): used by trim/collapse ports.
fn is_js_ws(ch: char) -> bool {
    matches!(
        ch,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

fn trim_js(text: &str) -> &str {
    text.trim_matches(is_js_ws)
}

/// `.replace(/\s+/gu, ' ')`
fn collapse_js_ws(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_ws = false;
    for ch in text.chars() {
        if is_js_ws(ch) {
            if !in_ws {
                out.push(' ');
                in_ws = true;
            }
        } else {
            out.push(ch);
            in_ws = false;
        }
    }
    out
}

/// Port of stripEmojiModifiers / INVISIBLE_RE:
/// /[­​‌‍⁠︀-️﻿]/gu
fn strip_invisible(text: &str) -> String {
    text.chars()
        .filter(|ch| {
            !matches!(
                ch,
                '\u{ad}' | '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{2060}' | '\u{fe00}'
                    ..='\u{fe0f}' | '\u{feff}'
            )
        })
        .collect()
}

/// Best-effort String.prototype.normalize('NFKC'). See module docs for scope.
fn nfkc_approx(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\u{a0}' | '\u{2000}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}' => {
                out.push(' ')
            }
            '\u{ff01}'..='\u{ff5e}' => {
                out.push(char::from_u32(ch as u32 - 0xff01 + 0x21).unwrap_or(ch))
            }
            '\u{fb00}' => out.push_str("ff"),
            '\u{fb01}' => out.push_str("fi"),
            '\u{fb02}' => out.push_str("fl"),
            '\u{fb03}' => out.push_str("ffi"),
            '\u{fb04}' => out.push_str("ffl"),
            '\u{2024}' => out.push('.'),
            other => out.push(other),
        }
    }
    out
}

/// JS String() coercion for the JSON values we encounter.
fn js_string(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => js_number_string(number),
        Value::Array(items) => items.iter().map(js_string).collect::<Vec<_>>().join(","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

fn js_number_string(number: &serde_json::Number) -> String {
    if let Some(integer) = number.as_i64() {
        return integer.to_string();
    }
    if let Some(unsigned) = number.as_u64() {
        return unsigned.to_string();
    }
    let float = number.as_f64().unwrap_or(0.0);
    if float.fract() == 0.0 && float.abs() < 9.007_199_254_740_992e15 {
        (float as i64).to_string()
    } else {
        float.to_string()
    }
}

/// Port of numericTimestamp (native-local-runtime.ts:74). The Date.parse
/// fallback is not ported; enqueue timestamps are epoch-ms strings here, and
/// the final fallback mirrors Date.now().
pub(crate) fn numeric_timestamp(value: &str) -> f64 {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return 0.0; // Number('') === 0, which is finite
    }
    if let Ok(numeric) = trimmed.parse::<f64>() {
        if numeric.is_finite() {
            return numeric;
        }
    }
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

/// Port of fnv1aPath (native-local-runtime.ts:86): FNV-1a 32-bit over UTF-16
/// code units, 8-char lowercase hex.
pub(crate) fn fnv1a_path(path: &str) -> Result<String, String> {
    if path.is_empty() {
        return Err("fnv1aPath: path must not be empty".to_string());
    }
    let mut hash: u32 = 0x811c_9dc5;
    for unit in path.encode_utf16() {
        hash ^= unit as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    Ok(format!("{hash:08x}"))
}

// ─────────────────────────────────────────────────────────────────────────────
// Path helpers (archive-import.ts + native-local-runtime.ts ports)
// ─────────────────────────────────────────────────────────────────────────────

fn path_parts(path: &str) -> Vec<&str> {
    path.split('/').filter(|part| !part.is_empty()).collect()
}

fn path_basename(path: &str) -> &str {
    path_parts(path).last().copied().unwrap_or(path)
}

fn path_ext(path: &str) -> String {
    let basename = path_basename(path);
    match basename.rfind('.') {
        Some(index) => basename[index..].to_lowercase(),
        None => String::new(),
    }
}

fn path_stem(path: &str) -> &str {
    let basename = path_basename(path);
    match basename.rfind('.') {
        Some(index) if index > 0 => &basename[..index],
        _ => basename,
    }
}

/// pathDirname returns '.' for ≤1 parts in TS; callers immediately convert
/// '.' to null, so this returns None for that case.
fn path_dirname(path: &str) -> Option<String> {
    let parts = path_parts(path);
    if parts.len() <= 1 {
        return None;
    }
    Some(parts[..parts.len() - 1].join("/"))
}

fn strip_zip_prefix<'a>(path: &'a str, prefix: &str) -> &'a str {
    if !prefix.is_empty() && path.starts_with(prefix) {
        &path[prefix.len()..]
    } else {
        path
    }
}

fn is_hidden_path(path: &str) -> bool {
    path_parts(path).iter().any(|part| part.starts_with('.'))
}

fn is_obsidian_skippable(path: &str) -> bool {
    path_parts(path)
        .iter()
        .any(|part| OBSIDIAN_SKIP_FILES.contains(part) || part.starts_with('.'))
}

fn detect_common_prefix(names: &[String], is_skippable: fn(&str) -> bool) -> String {
    let file_names: Vec<&String> = names
        .iter()
        .filter(|name| !name.ends_with('/') && !is_skippable(name))
        .collect();
    if file_names.is_empty() {
        return String::new();
    }
    let mut first_parts: HashSet<&str> = HashSet::new();
    for name in &file_names {
        let parts = path_parts(name);
        if parts.len() <= 1 {
            return String::new();
        }
        first_parts.insert(parts[0]);
    }
    if first_parts.len() == 1 {
        format!("{}/", first_parts.iter().next().unwrap())
    } else {
        String::new()
    }
}

/// Port of normalizeRelativePath (native-local-runtime.ts:3622).
pub(crate) fn normalize_relative_path(value: &str) -> Option<String> {
    let replaced = value.replace('\\', "/");
    let parts: Vec<&str> = replaced
        .split('/')
        .map(trim_js)
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    if parts.iter().any(|part| *part == "..") {
        return None;
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

/// Approximation of String.prototype.localeCompare for folder ordering:
/// case-insensitive primary weight, byte order tiebreak.
fn locale_compare_approx(left: &str, right: &str) -> std::cmp::Ordering {
    left.to_lowercase()
        .cmp(&right.to_lowercase())
        .then_with(|| left.cmp(right))
}

/// Port of expandBatchFolderPaths (native-local-runtime.ts:3632).
pub(crate) fn expand_batch_folder_paths(paths: &[String]) -> Vec<String> {
    let mut expanded: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for raw_path in paths {
        let Some(normalized) = normalize_relative_path(raw_path) else {
            continue;
        };
        let parts: Vec<&str> = normalized.split('/').collect();
        for index in 0..parts.len() {
            let joined = parts[..=index].join("/");
            if seen.insert(joined.clone()) {
                expanded.push(joined);
            }
        }
    }
    expanded.sort_by(|left, right| {
        let left_depth = left.split('/').count();
        let right_depth = right.split('/').count();
        left_depth
            .cmp(&right_depth)
            .then_with(|| locale_compare_approx(left, right))
    });
    expanded
}

/// Port of parentFolderPath (native-local-runtime.ts:3645).
fn parent_folder_path(relative_path: &str) -> Option<String> {
    let normalized = normalize_relative_path(relative_path)?;
    match normalized.rfind('/') {
        Some(index) if index > 0 => Some(normalized[..index].to_string()),
        _ => None,
    }
}

/// Port of pathBasename (native-local-runtime.ts:3652) — the workspace-folder
/// naming variant with the 'Untitled Folder' fallback.
fn folder_basename(relative_path: &str) -> String {
    match normalize_relative_path(relative_path) {
        Some(normalized) => normalized
            .split('/')
            .next_back()
            .filter(|part| !part.is_empty())
            .unwrap_or(&normalized)
            .to_string(),
        None => "Untitled Folder".to_string(),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Frontmatter helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Find the /\n---\s*\n/ frontmatter terminator inside `rest` (the text after
/// the leading '---'). Returns (match_index, match_len), greedy like the JS
/// regex: the closing newline is the LAST '\n' in the whitespace run.
fn frontmatter_terminator(rest: &str) -> Option<(usize, usize)> {
    let mut search_from = 0usize;
    while let Some(found) = rest[search_from..].find("\n---") {
        let pos = search_from + found;
        let after = pos + 4;
        let mut last_newline: Option<usize> = None;
        for (offset, ch) in rest[after..].char_indices() {
            if is_js_ws(ch) {
                if ch == '\n' {
                    last_newline = Some(after + offset);
                }
            } else {
                break;
            }
        }
        if let Some(newline) = last_newline {
            return Some((pos, newline + 1 - pos));
        }
        search_from = pos + 1;
    }
    None
}

/// Port of stripFrontmatter.
fn strip_frontmatter(markdown: &str) -> String {
    if !markdown.starts_with("---") {
        return markdown.to_string();
    }
    let rest = &markdown[3..];
    match frontmatter_terminator(rest) {
        Some((index, len)) => markdown[3 + index + len..]
            .trim_start_matches(is_js_ws)
            .to_string(),
        None => markdown.to_string(),
    }
}

fn frontmatter_block(markdown: &str) -> Option<&str> {
    if !markdown.starts_with("---") {
        return None;
    }
    let rest = &markdown[3..];
    let (index, _) = frontmatter_terminator(rest)?;
    Some(&rest[..index])
}

/// Port of extractFrontmatterTitle (/^title:\s*['"]?(.+?)['"]?\s*$/mu).
fn extract_frontmatter_title(markdown: &str) -> Option<String> {
    let frontmatter = frontmatter_block(markdown)?;
    for line in frontmatter.split('\n') {
        let Some(after) = line.strip_prefix("title:") else {
            continue;
        };
        // \s* — but `.` excludes line terminators so only this line matters.
        let after = after.trim_start_matches(is_js_ws);
        let mut chars: Vec<char> = after.chars().collect();
        // optional leading quote (lazy capture keeps ≥1 char, so only strip
        // when at least one char remains)
        if chars.len() >= 2 && (chars[0] == '\'' || chars[0] == '"') {
            chars.remove(0);
        }
        // trailing \s* then optional one quote
        while let Some(last) = chars.last() {
            if is_js_ws(*last) {
                chars.pop();
            } else {
                break;
            }
        }
        if chars.len() >= 2 {
            if let Some(last) = chars.last() {
                if *last == '\'' || *last == '"' {
                    chars.pop();
                }
            }
        }
        if chars.is_empty() {
            continue; // (.+?) requires at least one char; regex keeps searching
        }
        let captured: String = chars.into_iter().collect();
        let trimmed = trim_js(&captured);
        if trimmed.is_empty() {
            return None; // `title?.[1]?.trim() || null`
        }
        return Some(trimmed.to_string());
    }
    None
}

/// Port of extractFrontmatterTags. Mirrors the JS regex
/// /^tags:\s*(.*)$/mu where greedy \s* crosses newlines: the captured value
/// is the first non-whitespace line content after `tags:`.
fn extract_frontmatter_tags(markdown: &str, source_title: &str) -> Vec<ArchiveWikiLink> {
    let Some(frontmatter) = frontmatter_block(markdown) else {
        return Vec::new();
    };
    // Find the first line-start `tags:`.
    let mut tags_pos: Option<usize> = None;
    let mut line_start = 0usize;
    loop {
        if frontmatter[line_start..].starts_with("tags:") {
            tags_pos = Some(line_start);
            break;
        }
        match frontmatter[line_start..].find('\n') {
            Some(offset) => line_start += offset + 1,
            None => break,
        }
    }
    let Some(tags_pos) = tags_pos else {
        return Vec::new();
    };
    let after_colon = tags_pos + "tags:".len();
    // Greedy \s* (crosses newlines), then (.*) captures to end of line.
    let mut value_start = after_colon;
    for (offset, ch) in frontmatter[after_colon..].char_indices() {
        if is_js_ws(ch) {
            value_start = after_colon + offset + ch.len_utf8();
        } else {
            value_start = after_colon + offset;
            break;
        }
    }
    let value_end = frontmatter[value_start..]
        .find(['\n', '\r', '\u{2028}', '\u{2029}'])
        .map(|offset| value_start + offset)
        .unwrap_or(frontmatter.len());
    let raw_value = &frontmatter[value_start..value_end];
    let value = trim_js(raw_value);
    let mut tags: Vec<String> = Vec::new();
    if !value.is_empty() {
        let without_brackets = {
            let v = value.strip_prefix('[').unwrap_or(value);
            v.strip_suffix(']').unwrap_or(v)
        };
        for tag in without_brackets.split(',') {
            let cleaned = strip_edge_quotes(trim_js(tag));
            if !cleaned.is_empty() {
                tags.push(cleaned);
            }
        }
    } else {
        // value empty: everything after tags: up to the value line end was
        // whitespace. rest = frontmatter after the full regex match.
        let rest = &frontmatter[value_end.min(frontmatter.len())..];
        for line in rest.split('\n') {
            let cleaned = trim_js(line);
            if let Some(item) = cleaned.strip_prefix("- ") {
                let tag = strip_edge_quotes(trim_js(item));
                if !tag.is_empty() {
                    tags.push(tag);
                }
            } else if !cleaned.is_empty() && !cleaned.starts_with('#') {
                break;
            }
        }
    }
    tags.into_iter()
        .map(|tag| ArchiveWikiLink {
            source_title: source_title.to_string(),
            target_title: tag,
            display_text: None,
            source_context: None,
            is_tag: true,
        })
        .collect()
}

/// `.replace(/^['"]|['"]$/gu, '')` — strips ONE leading and ONE trailing quote.
fn strip_edge_quotes(text: &str) -> String {
    let mut chars: Vec<char> = text.chars().collect();
    if matches!(chars.first(), Some('\'') | Some('"')) {
        chars.remove(0);
    }
    if matches!(chars.last(), Some('\'') | Some('"')) {
        chars.pop();
    }
    chars.into_iter().collect()
}

// ─────────────────────────────────────────────────────────────────────────────
// Wikilink / hashtag extraction
// ─────────────────────────────────────────────────────────────────────────────

/// Port of mergeEmojiClassWikilinks.
fn merge_emoji_class_wikilinks(text: &str) -> String {
    merge_emoji_wikilink_re()
        .replace_all(text, |caps: &regex::Captures| {
            let target = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            let name = caps.get(2).map(|m| m.as_str()).unwrap_or("");
            if target.is_empty()
                || name.is_empty()
                || target.chars().any(|ch| ch.is_ascii_alphanumeric())
            {
                caps[0].to_string()
            } else {
                format!("[[{} {}]]", trim_js(target), trim_js(name))
            }
        })
        .into_owned()
}

/// Port of sourceLine: the trimmed line containing byte `index`, or None.
fn source_line(text: &str, index: usize) -> Option<String> {
    let start = text[..index].rfind('\n').map(|pos| pos + 1).unwrap_or(0);
    let end = text[index..]
        .find('\n')
        .map(|pos| pos + index)
        .unwrap_or(text.len());
    let line = trim_js(&text[start..end]);
    if line.is_empty() {
        None
    } else {
        Some(line.to_string())
    }
}

/// Port of extractObsidianWikilinks.
fn extract_obsidian_wikilinks(markdown: &str, source_title: &str) -> Vec<ArchiveWikiLink> {
    let cleaned = image_embed_re().replace_all(markdown, "").into_owned();
    let mut links = Vec::new();
    for caps in wikilink_re().captures_iter(&cleaned) {
        let whole = caps.get(0).unwrap();
        let inner = trim_js(caps.get(1).map(|m| m.as_str()).unwrap_or(""));
        if inner.is_empty() {
            continue;
        }
        let (raw_target, raw_display) = match inner.find('|') {
            Some(pipe) => {
                // JS split('|', 2): the second element stops at the next '|'.
                let rest = &inner[pipe + 1..];
                let display = match rest.find('|') {
                    Some(next) => &rest[..next],
                    None => rest,
                };
                (&inner[..pipe], Some(display))
            }
            None => (inner, None),
        };
        let target_title = trim_js(match raw_target.find('#') {
            Some(hash) => &raw_target[..hash],
            None => raw_target,
        });
        if target_title.is_empty() {
            continue;
        }
        let display_text = raw_display
            .map(trim_js)
            .filter(|text| !text.is_empty())
            .map(|text| text.to_string());
        links.push(ArchiveWikiLink {
            source_title: source_title.to_string(),
            target_title: target_title.to_string(),
            display_text,
            source_context: source_line(&cleaned, whole.start()),
            is_tag: false,
        });
    }
    links
}

/// Port of replaceObsidianWikilinksWithText.
fn replace_obsidian_wikilinks_with_text(markdown: &str) -> String {
    let without_embeds = image_embed_re().replace_all(markdown, "").into_owned();
    wikilink_re()
        .replace_all(&without_embeds, |caps: &regex::Captures| {
            let value = trim_js(caps.get(1).map(|m| m.as_str()).unwrap_or(""));
            let (target, display) = match value.find('|') {
                Some(pipe) => {
                    let rest = &value[pipe + 1..];
                    let display = match rest.find('|') {
                        Some(next) => &rest[..next],
                        None => rest,
                    };
                    (&value[..pipe], Some(display))
                }
                None => (value, None),
            };
            let clean_target = trim_js(match target.find('#') {
                Some(hash) => &target[..hash],
                None => target,
            });
            let display_trimmed = display.map(trim_js).filter(|text| !text.is_empty());
            trim_js(display_trimmed.unwrap_or(clean_target)).to_string()
        })
        .into_owned()
}

struct HashtagMatch {
    /// Byte index of the '#'.
    index: usize,
    tag: String,
}

/// Manual port of HASHTAG_RE /(?:^|(?<=\s))#([a-zA-Z][a-zA-Z0-9_/-]*)/gmu
/// (the regex crate has no lookbehind). `^` with m and `(?<=\s)` collapse to
/// "at index 0 or after JS whitespace" because \s includes line terminators.
fn find_hashtags(text: &str) -> Vec<HashtagMatch> {
    let mut matches = Vec::new();
    let mut prev_char: Option<char> = None;
    let mut iter = text.char_indices();
    while let Some((index, ch)) = iter.next() {
        if ch == '#' && (index == 0 || prev_char.map(is_js_ws).unwrap_or(false)) {
            let rest = &text[index + 1..];
            let mut tag_len = 0usize;
            for (pos, tag_char) in rest.char_indices() {
                let ok = if pos == 0 {
                    tag_char.is_ascii_alphabetic()
                } else {
                    tag_char.is_ascii_alphanumeric()
                        || tag_char == '_'
                        || tag_char == '/'
                        || tag_char == '-'
                };
                if ok {
                    tag_len = pos + 1;
                } else {
                    break;
                }
            }
            if tag_len > 0 {
                let tag = rest[..tag_len].to_string();
                matches.push(HashtagMatch { index, tag });
                // Consume the tag characters (regex lastIndex semantics).
                prev_char = Some(ch);
                for (pos, tag_char) in iter.by_ref() {
                    prev_char = Some(tag_char);
                    if pos + tag_char.len_utf8() - (index + 1) >= tag_len {
                        break;
                    }
                }
                continue;
            }
        }
        prev_char = Some(ch);
    }
    matches
}

/// Port of extractHashtags: code spans are masked (byte-for-byte, preserving
/// indices) before scanning; sourceLine reads from the ORIGINAL markdown.
fn extract_hashtags(markdown: &str, source_title: &str) -> Vec<ArchiveWikiLink> {
    let masked = code_block_re()
        .replace_all(markdown, |caps: &regex::Captures| " ".repeat(caps[0].len()))
        .into_owned();
    find_hashtags(&masked)
        .into_iter()
        .map(|m| ArchiveWikiLink {
            source_title: source_title.to_string(),
            target_title: m.tag,
            display_text: None,
            source_context: source_line(markdown, m.index),
            is_tag: true,
        })
        .collect()
}

/// Port of replaceHashtagsWithText (operates on the UNMASKED text, like TS).
fn replace_hashtags_with_text(markdown: &str) -> String {
    let matches = find_hashtags(markdown);
    if matches.is_empty() {
        return markdown.to_string();
    }
    let mut out = String::with_capacity(markdown.len());
    let mut cursor = 0usize;
    for m in matches {
        out.push_str(&markdown[cursor..m.index]);
        out.push_str(&m.tag);
        cursor = m.index + 1 + m.tag.len();
    }
    out.push_str(&markdown[cursor..]);
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// Zip extraction
// ─────────────────────────────────────────────────────────────────────────────

/// Port of fflate unzipSync usage: returns entries in central-directory order
/// (JS object insertion order); duplicate names keep first position, last
/// data. Any structural error maps to the single 'Invalid ZIP file' warning.
fn unzip_entries(zip_bytes: &[u8]) -> Result<Vec<(String, Vec<u8>)>, ()> {
    use std::io::Read;
    let cursor = std::io::Cursor::new(zip_bytes);
    let mut archive = zip::ZipArchive::new(cursor).map_err(|_| ())?;
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    let mut index_by_name: HashMap<String, usize> = HashMap::new();
    for index in 0..archive.len() {
        let mut file = archive.by_index(index).map_err(|_| ())?;
        let name = file.name().to_string();
        let mut data = Vec::new();
        file.read_to_end(&mut data).map_err(|_| ())?;
        match index_by_name.get(&name) {
            Some(&existing) => entries[existing].1 = data,
            None => {
                index_by_name.insert(name.clone(), entries.len());
                entries.push((name, data));
            }
        }
    }
    Ok(entries)
}

/// Port of parseArchiveImport (archive-import.ts:45).
pub(crate) fn parse_archive_import(
    source: ArchiveImportSource,
    zip_bytes: &[u8],
) -> ArchiveImportParseResult {
    let files = match unzip_entries(zip_bytes) {
        Ok(files) => files,
        Err(()) => {
            return ArchiveImportParseResult {
                documents: Vec::new(),
                wikilinks: Vec::new(),
                warnings: vec!["Invalid ZIP file".to_string()],
            }
        }
    };
    match source {
        ArchiveImportSource::Obsidian => parse_obsidian_zip(&files),
        ArchiveImportSource::Notion => parse_notion_zip(&files),
        ArchiveImportSource::Roam => parse_roam_zip(&files),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Obsidian parser
// ─────────────────────────────────────────────────────────────────────────────

pub(crate) fn parse_obsidian_zip(files: &[(String, Vec<u8>)]) -> ArchiveImportParseResult {
    let mut documents = Vec::new();
    let mut wikilinks = Vec::new();
    let mut warnings = Vec::new();
    let names: Vec<String> = files.iter().map(|(name, _)| name.clone()).collect();
    let is_logseq = names
        .iter()
        .any(|name| name == "logseq" || name.starts_with("logseq/"));
    if is_logseq {
        warnings.push("Detected Logseq vault format".to_string());
    }
    let prefix = detect_common_prefix(&names, is_obsidian_skippable);
    let mut total_size = 0usize;
    let mut file_count = 0usize;
    let mut skipped_extensions: HashSet<String> = HashSet::new();

    for (name, data) in files {
        if name.ends_with('/') {
            continue;
        }
        let rel_path = strip_zip_prefix(name, &prefix);
        if rel_path.is_empty() || is_obsidian_skippable(rel_path) {
            continue;
        }
        let ext = path_ext(rel_path);
        if ext != ".md" && ext != ".markdown" {
            if OBSIDIAN_SKIP_EXTENSIONS.contains(&ext.as_str()) {
                skipped_extensions.insert(ext);
            }
            continue;
        }
        total_size += data.len();
        if total_size > MAX_EXTRACTED_SIZE {
            warnings.push(format!(
                "Stopped processing: extracted size exceeded {}MB limit",
                MAX_EXTRACTED_SIZE / (1024 * 1024)
            ));
            break;
        }
        file_count += 1;
        if file_count > MAX_FILE_COUNT {
            warnings.push(format!(
                "Stopped processing: exceeded {MAX_FILE_COUNT} file limit"
            ));
            break;
        }

        let markdown = strip_invisible(&String::from_utf8_lossy(data));
        let markdown = merge_emoji_class_wikilinks(&markdown);
        let title =
            extract_frontmatter_title(&markdown).unwrap_or_else(|| path_stem(rel_path).to_string());
        wikilinks.extend(extract_obsidian_wikilinks(&markdown, &title));
        wikilinks.extend(extract_frontmatter_tags(&markdown, &title));
        wikilinks.extend(extract_hashtags(&markdown, &title));
        let clean_markdown = replace_obsidian_wikilinks_with_text(&markdown);
        let clean_markdown = replace_hashtags_with_text(&clean_markdown);
        let clean_markdown = strip_frontmatter(&clean_markdown);

        let mut folder_path = path_dirname(rel_path);
        let folder_is_daily_bucket = folder_path
            .as_deref()
            .map(|path| {
                let lowered = path.to_lowercase();
                lowered == "journals" || lowered == "daily notes"
            })
            .unwrap_or(false);
        if folder_path.is_none() || folder_is_daily_bucket {
            folder_path = detect_daily_note_folder(&title).or(folder_path);
        }
        documents.push(ArchiveImportedDocument {
            title,
            markdown: clean_markdown,
            folder_path,
            source_filename: Some(rel_path.to_string()),
        });
    }

    if !skipped_extensions.is_empty() {
        let mut sorted: Vec<String> = skipped_extensions.into_iter().collect();
        sorted.sort();
        warnings.push(format!(
            "Skipped unsupported file types: {}",
            sorted.join(", ")
        ));
    }
    if documents.is_empty() && warnings.is_empty() {
        warnings.push("No markdown files found in ZIP".to_string());
    }
    ArchiveImportParseResult {
        documents,
        wikilinks,
        warnings,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Notion parser
// ─────────────────────────────────────────────────────────────────────────────

fn strip_notion_uuid(name: &str) -> String {
    notion_uuid_re().replace(name, "").into_owned()
}

fn clean_notion_title(path: &str) -> String {
    trim_js(&strip_notion_uuid(path_stem(path))).to_string()
}

/// decodeURIComponent: strict percent decoding; None on malformed input
/// (caller keeps the raw path, mirroring the TS try/catch).
fn decode_uri_component(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = bytes.get(index + 1..index + 3)?;
            let high = (hex[0] as char).to_digit(16)?;
            let low = (hex[1] as char).to_digit(16)?;
            out.push((high * 16 + low) as u8);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Port of extractNotionLinks. The (?!https?:\/\/) lookahead and the `.md`
/// suffix requirement from the JS regex are applied as post-filters on the
/// raw (untrimmed) capture, which yields the same match set.
fn extract_notion_links(markdown: &str, source_title: &str) -> Vec<ArchiveWikiLink> {
    let mut links = Vec::new();
    for caps in notion_link_re().captures_iter(markdown) {
        let whole = caps.get(0).unwrap();
        let raw_href = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        let lowered = raw_href.to_lowercase();
        if lowered.starts_with("http://") || lowered.starts_with("https://") {
            continue;
        }
        if !lowered.ends_with(".md") {
            continue;
        }
        let display = trim_js(caps.get(1).map(|m| m.as_str()).unwrap_or("")).to_string();
        let href = trim_js(raw_href).to_string();
        let href = decode_uri_component(&href).unwrap_or(href);
        let target_title = clean_notion_title(path_basename(&href));
        if target_title.is_empty() {
            continue;
        }
        let display_text = if !display.is_empty() && display != target_title {
            Some(display)
        } else {
            None
        };
        links.push(ArchiveWikiLink {
            source_title: source_title.to_string(),
            target_title,
            display_text,
            source_context: source_line(markdown, whole.start()),
            is_tag: false,
        });
    }
    links
}

/// Port of parseCsvRows.
fn parse_csv_rows(csv: &str) -> Vec<Vec<String>> {
    let chars: Vec<char> = csv.chars().collect();
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut cell = String::new();
    let mut quoted = false;
    let mut index = 0usize;
    while index < chars.len() {
        let ch = chars[index];
        let next = chars.get(index + 1).copied();
        if quoted {
            if ch == '"' && next == Some('"') {
                cell.push('"');
                index += 1;
            } else if ch == '"' {
                quoted = false;
            } else {
                cell.push(ch);
            }
            index += 1;
            continue;
        }
        if ch == '"' {
            quoted = true;
        } else if ch == ',' {
            row.push(std::mem::take(&mut cell));
        } else if ch == '\n' {
            row.push(std::mem::take(&mut cell));
            rows.push(std::mem::take(&mut row));
        } else if ch != '\r' {
            cell.push(ch);
        }
        index += 1;
    }
    row.push(cell);
    if row.iter().any(|value| !value.is_empty()) || rows.is_empty() {
        rows.push(row);
    }
    rows
}

/// Port of csvToMarkdown.
fn csv_to_markdown(csv_content: &str, title: &str) -> String {
    let rows = parse_csv_rows(csv_content);
    if rows.is_empty() || rows[0].is_empty() {
        return format!("# {title}\n\n*Empty database*\n");
    }
    let header = &rows[0];
    let data_rows = &rows[1..];
    let mut lines: Vec<String> = vec![
        format!("# {title}"),
        String::new(),
        format!(
            "| {} |",
            header
                .iter()
                .map(|cell| trim_js(cell))
                .collect::<Vec<_>>()
                .join(" | ")
        ),
        format!(
            "| {} |",
            header.iter().map(|_| "---").collect::<Vec<_>>().join(" | ")
        ),
    ];
    let mut readable_rows: Vec<String> = Vec::new();
    for row in data_rows {
        let mut cells: Vec<String> = row.iter().map(|cell| trim_js(cell).to_string()).collect();
        while cells.len() < header.len() {
            cells.push(String::new());
        }
        cells.truncate(header.len());
        lines.push(format!(
            "| {} |",
            cells
                .iter()
                .map(|cell| cell.replace('|', "\\|"))
                .collect::<Vec<_>>()
                .join(" | ")
        ));
        let readable = cells
            .iter()
            .enumerate()
            .map(|(index, cell)| {
                let label = header.get(index).map(|h| trim_js(h)).unwrap_or("");
                if !label.is_empty() {
                    format!("{label}: {cell}")
                } else {
                    cell.clone()
                }
            })
            .filter(|entry| !entry.is_empty())
            .collect::<Vec<_>>()
            .join("; ");
        if !readable.is_empty() {
            readable_rows.push(format!("- {readable}"));
        }
    }
    if !readable_rows.is_empty() {
        lines.push(String::new());
        lines.extend(readable_rows);
    }
    format!("{}\n", lines.join("\n"))
}

pub(crate) fn parse_notion_zip(files: &[(String, Vec<u8>)]) -> ArchiveImportParseResult {
    let mut documents = Vec::new();
    let mut wikilinks = Vec::new();
    let mut warnings = Vec::new();
    let names: Vec<String> = files.iter().map(|(name, _)| name.clone()).collect();
    let prefix = detect_common_prefix(&names, is_hidden_path);
    let mut total_size = 0usize;
    let mut file_count = 0usize;
    let mut skipped_count = 0usize;

    for (name, data) in files {
        if name.ends_with('/') {
            continue;
        }
        let rel_path = strip_zip_prefix(name, &prefix);
        if rel_path.is_empty() || is_hidden_path(rel_path) {
            continue;
        }
        let ext = path_ext(rel_path);
        if !matches!(ext.as_str(), ".md" | ".markdown" | ".csv") {
            if NOTION_SKIP_EXTENSIONS.contains(&ext.as_str()) {
                skipped_count += 1;
            }
            continue;
        }
        total_size += data.len();
        if total_size > MAX_EXTRACTED_SIZE {
            warnings.push(format!(
                "Stopped processing: extracted size exceeded {}MB limit",
                MAX_EXTRACTED_SIZE / (1024 * 1024)
            ));
            break;
        }
        file_count += 1;
        if file_count > MAX_FILE_COUNT {
            warnings.push(format!(
                "Stopped processing: exceeded {MAX_FILE_COUNT} file limit"
            ));
            break;
        }

        let mut markdown = strip_invisible(&String::from_utf8_lossy(data));
        let cleaned_title = clean_notion_title(rel_path);
        let title = if cleaned_title.is_empty() {
            path_stem(rel_path).to_string()
        } else {
            cleaned_title
        };
        if ext == ".csv" {
            markdown = csv_to_markdown(&markdown, &title);
        } else {
            wikilinks.extend(extract_notion_links(&markdown, &title));
        }
        let dirname = path_dirname(rel_path);
        let folder_path = dirname.and_then(|dir| {
            let joined = dir
                .split('/')
                .map(strip_notion_uuid)
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join("/");
            if joined.is_empty() {
                None
            } else {
                Some(joined)
            }
        });
        documents.push(ArchiveImportedDocument {
            title,
            markdown,
            folder_path,
            source_filename: Some(rel_path.to_string()),
        });
    }

    if skipped_count > 0 {
        warnings.push(format!(
            "Skipped {skipped_count} non-importable files (images, etc.)"
        ));
    }
    if documents.is_empty() && warnings.is_empty() {
        warnings.push("No markdown files found in ZIP".to_string());
    }
    ArchiveImportParseResult {
        documents,
        wikilinks,
        warnings,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Roam parser
// ─────────────────────────────────────────────────────────────────────────────

fn strip_roam_brackets(title: &str) -> String {
    collapse_js_ws(&title.replace("[[", "").replace("]]", ""))
}

struct OutermostWikilink {
    /// char index of the opening '[['
    start: usize,
    /// char index just past the closing ']]'
    end: usize,
    target: String,
}

/// Port of findOutermostWikilinks (char-index based; the TS uses UTF-16
/// indices, consistently within the same string, so results agree).
fn find_outermost_wikilinks(text: &[char]) -> Vec<OutermostWikilink> {
    let mut results = Vec::new();
    let mut index = 0usize;
    while index + 1 < text.len() {
        if !(text[index] == '[' && text[index + 1] == '[') {
            index += 1;
            continue;
        }
        let start = index;
        let mut depth = 1usize;
        index += 2;
        while index + 1 < text.len() && depth > 0 {
            if text[index] == '[' && text[index + 1] == '[' {
                depth += 1;
                index += 2;
            } else if text[index] == ']' && text[index + 1] == ']' {
                depth -= 1;
                index += 2;
            } else {
                index += 1;
            }
        }
        if depth == 0 {
            let inner: String = text[start + 2..index - 2].iter().collect();
            let target = trim_js(&strip_roam_brackets(&inner)).to_string();
            results.push(OutermostWikilink {
                start,
                end: index,
                target,
            });
        } else {
            index = start + 2;
        }
    }
    results
}

fn extract_roam_links_recursive(blocks: &[Value], source_title: &str) -> Vec<ArchiveWikiLink> {
    let mut links = Vec::new();
    for block in blocks {
        let Some(value) = block.as_object() else {
            continue;
        };
        let raw = match value.get("string") {
            None | Some(Value::Null) => String::new(),
            Some(other) => js_string(other),
        };
        let text = strip_invisible(&raw);
        if !text.is_empty() {
            let text = merge_emoji_class_wikilinks(&text);
            let chars: Vec<char> = text.chars().collect();
            let context = {
                let trimmed = trim_js(&text);
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            };
            for wikilink in find_outermost_wikilinks(&chars) {
                if wikilink.target.is_empty()
                    || SYSTEM_ROAM_PAGES.contains(&wikilink.target.as_str())
                {
                    continue;
                }
                links.push(ArchiveWikiLink {
                    source_title: source_title.to_string(),
                    target_title: wikilink.target,
                    display_text: None,
                    source_context: context.clone(),
                    is_tag: false,
                });
            }
            for m in find_hashtags(&text) {
                links.push(ArchiveWikiLink {
                    source_title: source_title.to_string(),
                    target_title: m.tag,
                    display_text: None,
                    source_context: context.clone(),
                    is_tag: true,
                });
            }
        }
        if let Some(children) = value.get("children").and_then(Value::as_array) {
            links.extend(extract_roam_links_recursive(children, source_title));
        }
    }
    links
}

/// Port of cleanRoamBlockText.
fn clean_roam_block_text(text: &str) -> (String, bool, bool) {
    let is_todo = text.contains("{{[[TODO]]}}");
    let is_done = text.contains("{{[[DONE]]}}");
    let result = todo_marker_re().replace_all(text, "");
    let result = done_marker_re().replace_all(&result, "");
    let result = block_ref_re().replace_all(&result, "");
    let result = embed_re().replace_all(&result, "").into_owned();
    let chars: Vec<char> = result.chars().collect();
    let mut chars_out = chars.clone();
    for wikilink in find_outermost_wikilinks(&chars).into_iter().rev() {
        let replacement: Vec<char> = wikilink.target.chars().collect();
        chars_out.splice(wikilink.start..wikilink.end, replacement);
    }
    let result: String = chars_out.into_iter().collect();
    let result = replace_hashtags_with_text(&result);
    let result = attribute_re()
        .replace_all(&result, "**${1}:** ${2}")
        .into_owned();
    (trim_js(&result).to_string(), is_todo, is_done)
}

fn roam_blocks_to_markdown(blocks: &[Value], indent: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let prefix = "  ".repeat(indent);
    for block in blocks {
        let Some(value) = block.as_object() else {
            continue;
        };
        let raw = match value.get("string") {
            None | Some(Value::Null) => String::new(),
            Some(other) => js_string(other),
        };
        let text = trim_js(&strip_invisible(&raw)).to_string();
        if text.is_empty() {
            continue;
        }
        let text = merge_emoji_class_wikilinks(&text);
        let (cleaned, is_todo, is_done) = clean_roam_block_text(&text);
        if !cleaned.is_empty() {
            let marker = if is_todo || is_done {
                if is_done {
                    "[x] "
                } else {
                    "[ ] "
                }
            } else {
                ""
            };
            lines.push(format!("{prefix}- {marker}{cleaned}"));
        }
        if let Some(children) = value.get("children").and_then(Value::as_array) {
            lines.extend(roam_blocks_to_markdown(children, indent + 1));
        }
    }
    lines
}

pub(crate) fn parse_roam_zip(files: &[(String, Vec<u8>)]) -> ArchiveImportParseResult {
    let mut documents: Vec<ArchiveImportedDocument> = Vec::new();
    let mut wikilinks: Vec<ArchiveWikiLink> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut json_files: Vec<&String> = files
        .iter()
        .map(|(name, _)| name)
        .filter(|name| name.to_lowercase().ends_with(".json") && !name.starts_with("__MACOSX"))
        .collect();
    json_files.sort();
    if json_files.is_empty() {
        return ArchiveImportParseResult {
            documents,
            wikilinks,
            warnings: vec!["No JSON file found in ZIP".to_string()],
        };
    }
    if json_files.len() > 1 {
        warnings.push(format!(
            "Multiple JSON files found, using first: {}",
            json_files[0]
        ));
    }
    let json_file = json_files[0].clone();
    let raw = files
        .iter()
        .find(|(name, _)| *name == json_file)
        .map(|(_, data)| data);
    let Some(raw) = raw else {
        return ArchiveImportParseResult {
            documents,
            wikilinks,
            warnings: vec!["No JSON file found in ZIP".to_string()],
        };
    };
    if raw.len() > MAX_EXTRACTED_SIZE {
        return ArchiveImportParseResult {
            documents,
            wikilinks,
            warnings: vec![format!(
                "JSON file exceeds maximum size of {}MB",
                MAX_EXTRACTED_SIZE / (1024 * 1024)
            )],
        };
    }
    let decoded = String::from_utf8_lossy(raw).into_owned();
    let pages: Value = match serde_json::from_str(&decoded) {
        Ok(pages) => pages,
        Err(error) => {
            return ArchiveImportParseResult {
                documents,
                wikilinks,
                warnings: vec![format!("Invalid JSON in {json_file}: {error}")],
            }
        }
    };
    let Some(pages) = pages.as_array() else {
        return ArchiveImportParseResult {
            documents,
            wikilinks,
            warnings: vec!["Roam export JSON must be an array of page objects".to_string()],
        };
    };
    let mut page_objects: &[Value] = pages;
    if page_objects.len() > MAX_PAGE_COUNT {
        warnings.push(format!(
            "Truncated to {MAX_PAGE_COUNT} pages (export contained {})",
            page_objects.len()
        ));
        page_objects = &page_objects[..MAX_PAGE_COUNT];
    }

    for page in page_objects {
        let Some(value) = page.as_object() else {
            continue;
        };
        let raw_title = match value.get("title") {
            None | Some(Value::Null) => String::new(),
            Some(other) => js_string(other),
        };
        let title = trim_js(&strip_roam_brackets(&strip_invisible(&raw_title))).to_string();
        if title.is_empty() {
            continue;
        }
        let empty = Vec::new();
        let children = value
            .get("children")
            .and_then(Value::as_array)
            .unwrap_or(&empty);
        wikilinks.extend(extract_roam_links_recursive(children, &title));
        let lines = roam_blocks_to_markdown(children, 0);
        let markdown = if !lines.is_empty() {
            format!("# {title}\n\n{}", lines.join("\n"))
        } else {
            format!("# {title}\n")
        };
        documents.push(ArchiveImportedDocument {
            folder_path: detect_daily_note_folder(&title),
            source_filename: Some(format!("{title}.json")),
            title,
            markdown,
        });
    }
    if documents.is_empty() && warnings.is_empty() {
        warnings.push("No pages found in Roam export".to_string());
    }
    ArchiveImportParseResult {
        documents,
        wikilinks,
        warnings,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Daily-note folder detection
// ─────────────────────────────────────────────────────────────────────────────

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Port of detectDailyNoteFolder. The Roam branch mirrors the V8 legacy date
/// parser: days 1..=31 are accepted and roll over into the next month
/// ("February 30" → March); anything else is Invalid Date → None.
pub(crate) fn detect_daily_note_folder(title: &str) -> Option<String> {
    let trimmed = title.trim();
    if let Some(caps) = roam_date_re().captures(trimmed) {
        let month_name = caps.get(1).unwrap().as_str();
        let day: i64 = caps[2].parse().ok()?;
        let year: i64 = caps[4].parse().ok()?;
        if !(1..=31).contains(&day) {
            return None;
        }
        let month0 = MONTH_NAMES.iter().position(|m| *m == month_name)? as i64;
        // Roll over excess days like the JS Date would.
        let mut y = year;
        let mut m = month0; // 0-based
        let mut d = day;
        loop {
            let dim = days_in_month(y, (m + 1) as u32) as i64;
            if d <= dim {
                break;
            }
            d -= dim;
            m += 1;
            if m == 12 {
                m = 0;
                y += 1;
            }
        }
        return Some(format!("Daily Notes/{y}/{:02}-{month_name}", m + 1));
    }
    if let Some(caps) = iso_date_re().captures(trimmed) {
        let year: i64 = caps[1].parse().ok()?;
        let month: u32 = caps[2].parse().ok()?;
        let day: u32 = caps[3].parse().ok()?;
        // Date.UTC round-trip check rejects out-of-range components.
        if (1..=12).contains(&month) && day >= 1 && day <= days_in_month(year, month) {
            let month_name = MONTH_NAMES[(month - 1) as usize];
            return Some(format!("Daily Notes/{year}/{month:02}-{month_name}"));
        }
    }
    None
}

// ─────────────────────────────────────────────────────────────────────────────
// Title lookup keys + resolution (archive-import.ts archiveTitleLookupKeys,
// native-local-runtime.ts resolveArchiveTitle)
// ─────────────────────────────────────────────────────────────────────────────

pub(crate) fn archive_title_lookup_keys(title: &str) -> Vec<String> {
    let normalized = nfkc_approx(&strip_invisible(title));
    let normalized = trim_js(&normalized).replace("[[", "").replace("]]", "");
    let normalized = collapse_js_ws(&normalized).to_lowercase();
    let mut keys: Vec<String> = vec![normalized.clone()];
    if normalized.contains('\u{2019}') {
        keys.push(normalized.replace('\u{2019}', "'"));
    }
    if normalized.contains('\'') {
        keys.push(normalized.replace('\'', "\u{2019}"));
    }
    if normalized.contains('\u{2013}') {
        keys.push(normalized.replace('\u{2013}', "-"));
    }
    if normalized.contains('-') {
        keys.push(normalized.replace('-', "\u{2013}"));
    }
    if normalized.contains('\u{2014}') {
        keys.push(normalized.replace('\u{2014}', "-"));
    }
    if normalized.contains('-') {
        keys.push(normalized.replace('-', "\u{2014}"));
    }
    let mut seen: HashSet<String> = HashSet::new();
    keys.into_iter()
        .filter(|key| !key.is_empty() && seen.insert(key.clone()))
        .collect()
}

pub(crate) fn resolve_archive_title(
    title_to_document_id: &HashMap<String, String>,
    title: &str,
) -> Option<String> {
    for key in archive_title_lookup_keys(title) {
        if let Some(document_id) = title_to_document_id.get(&key) {
            return Some(document_id.clone());
        }
    }
    None
}

/// Port of archiveFolderIdForDocument (native-local-runtime.ts:3479).
fn archive_folder_id_for_document(
    document: &ArchiveImportedDocument,
    root_folder_name: Option<&str>,
    folder_map: &HashMap<String, String>,
) -> Option<String> {
    let normalized_root = root_folder_name.and_then(normalize_relative_path);
    let normalized_folder = document
        .folder_path
        .as_deref()
        .and_then(normalize_relative_path);
    let folder_path = match (normalized_root, normalized_folder) {
        (Some(root), Some(folder)) => Some(format!("{root}/{folder}")),
        (root, folder) => folder.or(root),
    };
    folder_path.and_then(|path| folder_map.get(&path).cloned())
}

// ─────────────────────────────────────────────────────────────────────────────
// Import plan (the pure resolution core of importVaultArchive)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub(crate) struct PlannedFolder {
    pub(crate) folder_id: String,
    pub(crate) name: String,
    pub(crate) parent_id: Option<String>,
    pub(crate) order: f64,
}

#[derive(Debug, Clone)]
pub(crate) struct PlannedAssignment {
    pub(crate) document: ArchiveImportedDocument,
    pub(crate) document_id: String,
    pub(crate) parent_id: Option<String>,
    pub(crate) is_tag_hub: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct PlannedWire {
    pub(crate) wire_id: String,
    pub(crate) source_document_id: String,
    pub(crate) target_document_id: String,
    pub(crate) predicate: &'static str,
    pub(crate) source_context: Option<String>,
    pub(crate) source_title: String,
    pub(crate) target_title: String,
    /// archiveSourceBlockId candidate strings, in TS order
    /// (displayText, targetTitle, sourceContext), already trim-filtered.
    pub(crate) block_candidates: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct VaultImportPlan {
    pub(crate) folders: Vec<PlannedFolder>,
    pub(crate) assignments: Vec<PlannedAssignment>,
    pub(crate) tags_created: usize,
    pub(crate) wires: Vec<PlannedWire>,
    /// Unresolved wikilink target titles in insertion order (JS Set).
    pub(crate) unresolved: Vec<String>,
}

/// Pure resolution core of importVaultArchive: folder expansion, deterministic
/// document IDs, tag-hub preparation, and wikilink → wire resolution. The
/// handler executes this plan through document_ops/workspace_ops.
pub(crate) fn build_vault_import_plan(
    parsed: &ArchiveImportParseResult,
    root_folder_name: Option<&str>,
    operation_id: &str,
    enqueue_ms: f64,
) -> Result<VaultImportPlan, String> {
    // ── createArchiveImportFolders
    let mut raw_paths: Vec<String> = Vec::new();
    let mut raw_seen: HashSet<String> = HashSet::new();
    for document in &parsed.documents {
        if let Some(normalized) = document
            .folder_path
            .as_deref()
            .and_then(normalize_relative_path)
        {
            if raw_seen.insert(normalized.clone()) {
                raw_paths.push(normalized);
            }
        }
    }
    let mut folder_paths = expand_batch_folder_paths(&raw_paths);
    let normalized_root = root_folder_name.and_then(normalize_relative_path);
    if let Some(root) = &normalized_root {
        let mut wrapped: Vec<String> = vec![root.clone()];
        wrapped.extend(
            folder_paths
                .iter()
                .map(|folder_path| format!("{root}/{folder_path}")),
        );
        folder_paths = expand_batch_folder_paths(&wrapped);
    }
    let mut folder_map: HashMap<String, String> = HashMap::new();
    let mut folders: Vec<PlannedFolder> = Vec::new();
    for (index, folder_path) in folder_paths.iter().enumerate() {
        let parent_path = parent_folder_path(folder_path);
        let folder_id = format!("{operation_id}-folder-{}", fnv1a_path(folder_path)?);
        folder_map.insert(folder_path.clone(), folder_id.clone());
        folders.push(PlannedFolder {
            folder_id,
            name: folder_basename(folder_path),
            parent_id: parent_path.and_then(|path| folder_map.get(&path).cloned()),
            order: enqueue_ms + index as f64,
        });
    }

    // ── document assignments (deterministic IDs)
    let mut assignments: Vec<PlannedAssignment> = Vec::new();
    for document in &parsed.documents {
        let path_key = document
            .source_filename
            .clone()
            .unwrap_or_else(|| document.title.clone());
        if path_key.is_empty() {
            return Err(
                "import.vault: document has neither sourceFilename nor title; cannot derive deterministic ID"
                    .to_string(),
            );
        }
        assignments.push(PlannedAssignment {
            document: document.clone(),
            document_id: format!("{operation_id}-doc-{}", fnv1a_path(&path_key)?),
            parent_id: archive_folder_id_for_document(document, root_folder_name, &folder_map),
            is_tag_hub: false,
        });
    }
    let mut title_to_document_id: HashMap<String, String> = HashMap::new();
    for assignment in &assignments {
        for key in archive_title_lookup_keys(&assignment.document.title) {
            title_to_document_id
                .entry(key)
                .or_insert_with(|| assignment.document_id.clone());
        }
    }

    // ── prepareArchiveTagHubDocuments
    let mut tag_targets: Vec<String> = Vec::new();
    let mut tag_seen: HashSet<String> = HashSet::new();
    for link in &parsed.wikilinks {
        if link.is_tag
            && !link.target_title.is_empty()
            && tag_seen.insert(link.target_title.clone())
        {
            tag_targets.push(link.target_title.clone());
        }
    }
    tag_targets.sort();
    let unresolved_tags: Vec<String> = tag_targets
        .into_iter()
        .filter(|tag| resolve_archive_title(&title_to_document_id, tag).is_none())
        .collect();
    let mut tags_created = 0usize;
    if !unresolved_tags.is_empty() {
        let tags_path = match &normalized_root {
            Some(root) => format!("{root}/Tags"),
            None => "Tags".to_string(),
        };
        if !folder_map.contains_key(&tags_path) {
            let parent_path = parent_folder_path(&tags_path);
            let folder_id = format!("{operation_id}-folder-{}", fnv1a_path(&tags_path)?);
            folder_map.insert(tags_path.clone(), folder_id.clone());
            folders.push(PlannedFolder {
                folder_id,
                name: "Tags".to_string(),
                parent_id: parent_path.and_then(|path| folder_map.get(&path).cloned()),
                order: enqueue_ms,
            });
        }
        let parent_id = folder_map.get(&tags_path).cloned();
        for tag in &unresolved_tags {
            let document_id = format!("{operation_id}-doc-{}", fnv1a_path(tag)?);
            for key in archive_title_lookup_keys(tag) {
                title_to_document_id
                    .entry(key)
                    .or_insert_with(|| document_id.clone());
            }
            assignments.push(PlannedAssignment {
                document: ArchiveImportedDocument {
                    title: tag.clone(),
                    markdown: format!("# {tag}"),
                    folder_path: Some(tags_path.clone()),
                    source_filename: None,
                },
                document_id,
                parent_id: parent_id.clone(),
                is_tag_hub: true,
            });
        }
        tags_created = unresolved_tags.len();
    }

    // ── wikilink → wire resolution
    let mut source_title_to_document_id: HashMap<String, String> = HashMap::new();
    for assignment in assignments.iter().filter(|a| !a.is_tag_hub) {
        for key in archive_title_lookup_keys(&assignment.document.title) {
            source_title_to_document_id
                .entry(key)
                .or_insert_with(|| assignment.document_id.clone());
        }
    }
    let mut unresolved: Vec<String> = Vec::new();
    let mut unresolved_seen: HashSet<String> = HashSet::new();
    let mut wires: Vec<PlannedWire> = Vec::new();
    for wikilink in &parsed.wikilinks {
        let source_document_id =
            resolve_archive_title(&source_title_to_document_id, &wikilink.source_title);
        let target_document_id =
            resolve_archive_title(&title_to_document_id, &wikilink.target_title);
        let (Some(source_document_id), Some(target_document_id)) =
            (source_document_id, target_document_id)
        else {
            if unresolved_seen.insert(wikilink.target_title.clone()) {
                unresolved.push(wikilink.target_title.clone());
            }
            continue;
        };
        if source_document_id == target_document_id {
            continue;
        }
        let predicate = if wikilink.is_tag {
            "exemplifies"
        } else {
            "isWiredTo"
        };
        let wire_key =
            format!("{operation_id}:{source_document_id}:{target_document_id}:{predicate}");
        let block_candidates: Vec<String> = [
            wikilink.display_text.as_deref(),
            Some(wikilink.target_title.as_str()),
            wikilink.source_context.as_deref(),
        ]
        .into_iter()
        .flatten()
        .filter(|candidate| !candidate.trim().is_empty())
        .map(|candidate| candidate.to_string())
        .collect();
        wires.push(PlannedWire {
            wire_id: format!("{operation_id}-w-{}", fnv1a_path(&wire_key)?),
            source_document_id,
            target_document_id,
            predicate,
            source_context: wikilink.source_context.clone(),
            source_title: wikilink.source_title.clone(),
            target_title: wikilink.target_title.clone(),
            block_candidates,
        });
    }

    Ok(VaultImportPlan {
        folders,
        assignments,
        tags_created,
        wires,
        unresolved,
    })
}

/// The "N wikilinks could not be resolved" warning, verbatim.
pub(crate) fn unresolved_links_warning(unresolved: &[String]) -> String {
    let mut sorted: Vec<&String> = unresolved.iter().collect();
    sorted.sort();
    let preview = sorted
        .iter()
        .take(10)
        .map(|title| format!("[[{title}]]"))
        .collect::<Vec<_>>()
        .join(", ");
    let suffix = if unresolved.len() > 10 {
        format!(" ... and {} more", unresolved.len() - 10)
    } else {
        String::new()
    };
    format!(
        "{} wikilinks could not be resolved: {preview}{suffix}",
        unresolved.len()
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// Import result envelope (createImportResult / importResultEnvelope)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub(crate) struct LocalImportResult {
    pub(crate) status: &'static str,
    pub(crate) documents_created: usize,
    pub(crate) folders_created: usize,
    pub(crate) wires_created: usize,
    pub(crate) tags_created: usize,
    pub(crate) unresolved_links: usize,
    pub(crate) document_ids: Vec<String>,
    pub(crate) warnings: Vec<String>,
    pub(crate) errors: Vec<String>,
}

pub(crate) fn create_import_result() -> LocalImportResult {
    LocalImportResult {
        status: "complete",
        documents_created: 0,
        folders_created: 0,
        wires_created: 0,
        tags_created: 0,
        unresolved_links: 0,
        document_ids: Vec::new(),
        warnings: Vec::new(),
        errors: Vec::new(),
    }
}

/// Port of importResultEnvelope: DUAL snake+camel keys.
pub(crate) fn import_result_envelope(result: &LocalImportResult) -> Value {
    json!({
        "status": result.status,
        "documents_created": result.documents_created,
        "documentsCreated": result.documents_created,
        "folders_created": result.folders_created,
        "foldersCreated": result.folders_created,
        "wires_created": result.wires_created,
        "wiresCreated": result.wires_created,
        "tags_created": result.tags_created,
        "tagsCreated": result.tags_created,
        "unresolved_links": result.unresolved_links,
        "unresolvedLinks": result.unresolved_links,
        "document_ids": result.document_ids,
        "documentIds": result.document_ids,
        "warnings": result.warnings,
        "errors": result.errors,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Payload helpers (JS ?? / String() semantics, mirroring workspace_ops)
// ─────────────────────────────────────────────────────────────────────────────

fn object_payload(value: &Value) -> JsonMap<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

/// First key present with a non-null value (a JS `a ?? b ?? …` chain).
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
        Value::Number(number) => Some(js_number_string(number)),
        _ => None,
    }
}

/// Port of archiveImportSource (native-local-runtime.ts:3465).
pub(crate) fn archive_import_source(value: Option<&Value>) -> Result<ArchiveImportSource, String> {
    let normalized = js_string(value.unwrap_or(&Value::Null))
        .trim()
        .to_lowercase();
    match normalized.as_str() {
        "obsidian" => Ok(ArchiveImportSource::Obsidian),
        "notion" => Ok(ArchiveImportSource::Notion),
        "roam" => Ok(ArchiveImportSource::Roam),
        _ => Err(format!(
            "unsupported archive import source: {}",
            if normalized.is_empty() {
                "missing"
            } else {
                normalized.as_str()
            }
        )),
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Impure handler
// ─────────────────────────────────────────────────────────────────────────────

fn decode_bytes_base64(value: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(value)
        .map_err(|error| format!("{error}"))
}

fn cleanup_pending_archive(
    app: &AppHandle,
    graph_id: &str,
    pending_path: &str,
) -> Result<(), String> {
    crate::pending_upload_service::cleanup_pending_upload(
        app.clone(),
        crate::pending_upload_service::PendingUploadFileInput {
            graph_id: Some(graph_id.to_string()),
            pending_path: pending_path.to_string(),
        },
    )
}

/// Port of archiveSourceBlockId (native-local-runtime.ts:2743): scan the
/// persisted blocks of the (just-written) source document for the first block
/// whose text contains any candidate string. Errors degrade to None, mirroring
/// the TS missing-record path.
fn archive_source_block_id(
    app: &AppHandle,
    graph_id: &str,
    document_id: &str,
    candidates: &[String],
) -> Option<String> {
    if candidates.is_empty() {
        return None;
    }
    let graph_dir = crate::graph_paths::existing_graph_dir(app, graph_id).ok()?;
    let blocks_path =
        crate::ydoc_paths::document_ydoc_dir(&graph_dir, document_id).join("blocks.json");
    let raw = std::fs::read_to_string(blocks_path).ok()?;
    let blocks: Value = serde_json::from_str(&raw).ok()?;
    for block in blocks.as_array()? {
        let value = object_payload(block);
        let block_text = string_value(pick(&value, &["content", "text"])).unwrap_or_default();
        if block_text.is_empty() {
            continue;
        }
        if candidates
            .iter()
            .any(|candidate| block_text.contains(candidate.as_str()))
        {
            return string_value(pick(&value, &["id", "blockId", "block_id"]));
        }
    }
    None
}

/// Port of importVaultArchive (native-local-runtime.ts:2224).
pub(crate) async fn apply(app: &AppHandle, operation: &CrdtOperation) -> Result<Value, String> {
    apply_classified(app, operation)
        .await
        .map_err(ApplyOperationError::into_message)
}

fn promote_partial_import_error(
    error: ApplyOperationError,
    durable_started: bool,
) -> ApplyOperationError {
    match error {
        ApplyOperationError::RetryableAfterHotCommit(_) => error,
        ApplyOperationError::Terminal(message) if durable_started => {
            ApplyOperationError::retryable_after_hot_commit(message)
        }
        ApplyOperationError::Terminal(_) => error,
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VaultFailurePoint {
    BeforeDocument,
    BeforeWire,
}

#[cfg(test)]
static FAIL_NEXT_VAULT_STEP: std::sync::OnceLock<
    std::sync::Mutex<Option<(String, VaultFailurePoint)>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
pub(crate) fn fail_next_vault_step_for_test(
    operation_id: impl Into<String>,
    point: VaultFailurePoint,
) {
    *FAIL_NEXT_VAULT_STEP
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((operation_id.into(), point));
}

#[cfg(test)]
fn maybe_fail_vault_step_for_test(
    operation_id: &str,
    point: VaultFailurePoint,
) -> Result<(), ApplyOperationError> {
    let mut pending = FAIL_NEXT_VAULT_STEP
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if pending
        .as_ref()
        .is_some_and(|(expected_id, expected_point)| {
            expected_id == operation_id && *expected_point == point
        })
    {
        pending.take();
        return Err(ApplyOperationError::terminal(format!(
            "injected import.vault failure at {point:?} for {operation_id}"
        )));
    }
    Ok(())
}

fn finish_vault_completion(
    app: &AppHandle,
    operation: &CrdtOperation,
    graph_id: &str,
    pending_archive_path: Option<&str>,
    envelope: Value,
) -> ApplyOperationResult<Value> {
    let entry = crate::operation_completion_ledger::OperationCompletionEntry {
        schema_version: 1,
        operation_id: operation.operation_id.clone(),
        kind: "import.vault".to_string(),
        graph_id: Some(graph_id.to_string()),
        completed_at: operation.enqueue_timestamp.clone(),
        payload_hash: None,
        result: Some(envelope.clone()),
    };
    crate::operation_completion_ledger::append_completion_entry(app, entry)
        .map_err(ApplyOperationError::retryable_after_hot_commit)?;
    if let Some(pending) = pending_archive_path {
        if let Err(error) = cleanup_pending_archive(app, graph_id, pending) {
            log::warn!("[import.vault] pending archive cleanup failed: {error}");
        }
    }
    Ok(envelope)
}

pub(crate) async fn apply_classified(
    app: &AppHandle,
    operation: &CrdtOperation,
) -> ApplyOperationResult<Value> {
    // Defer periodic durable flushes for the duration of this import, and fire
    // one forced flush when it finishes (see cell_durability::ImportGuard).
    let _import_guard = crate::cell_durability::import_guard();
    let graph_id = operation.graph_id.clone();
    let operation_id = operation.operation_id.clone();
    let enqueue_timestamp = operation.enqueue_timestamp.clone();
    let value = object_payload(&operation.payload);
    let source_type = archive_import_source(pick(&value, &["sourceType", "source_type"]))?;
    let zip_base64 = string_value(pick(
        &value,
        &["zipBase64", "zip_base64", "dataBase64", "data_base64"],
    ));
    let pending_archive_path = string_value(pick(
        &value,
        &["pendingArchivePath", "pending_archive_path"],
    ));
    if zip_base64.is_none() && pending_archive_path.is_none() {
        return Err(ApplyOperationError::terminal(
            "import.vault: zipBase64 or pendingArchivePath is required",
        ));
    }
    let root_folder_name = string_value(pick(&value, &["folderName", "folder_name"]));

    // Tier B replay guard: if this operation completed previously, return the
    // cached envelope without re-touching the filesystem.
    match crate::operation_completion_ledger::completion_entry_for(app, &operation_id) {
        Ok(Some(entry)) if entry.kind == "import.vault" => {
            let mut cached = entry
                .result
                .as_ref()
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            // Best-effort cleanup of any pending vault file before
            // short-circuiting.
            if let Some(pending) = &pending_archive_path {
                let _ = cleanup_pending_archive(app, &graph_id, pending);
            }
            cached.insert("replayed".to_string(), json!(true));
            return Ok(Value::Object(cached));
        }
        Ok(Some(entry)) => {
            return Err(ApplyOperationError::terminal(format!(
                "completion ledger operation {operation_id} belongs to {}, not import.vault",
                entry.kind
            )));
        }
        Ok(None) => {}
        Err(error) => {
            return Err(ApplyOperationError::retryable_after_hot_commit(format!(
                "read import.vault completion ledger: {error}"
            )));
        }
    }

    let bytes: Vec<u8> = if let Some(pending) = &pending_archive_path {
        let pending_upload = crate::pending_upload_service::read_pending_upload_file(
            app.clone(),
            crate::pending_upload_service::PendingUploadFileInput {
                graph_id: Some(graph_id.clone()),
                pending_path: pending.clone(),
            },
        )
        .map_err(ApplyOperationError::retryable_after_hot_commit)?;
        decode_bytes_base64(&pending_upload.data_base64)?
    } else {
        decode_bytes_base64(zip_base64.as_deref().unwrap_or(""))?
    };
    let parsed = parse_archive_import(source_type, &bytes);

    let mut import_result = create_import_result();
    import_result.warnings.extend(parsed.warnings.clone());
    if parsed.documents.is_empty() {
        let envelope = import_result_envelope(&import_result);
        return finish_vault_completion(
            app,
            operation,
            &graph_id,
            pending_archive_path.as_deref(),
            envelope,
        );
    }

    let enqueue_ms = numeric_timestamp(&enqueue_timestamp);
    let plan = build_vault_import_plan(
        &parsed,
        root_folder_name.as_deref(),
        &operation_id,
        enqueue_ms,
    )?;

    // ── createArchiveImportFolders + the tag-hub Tags folder. The TS does
    // these inside transactWorkspace; here each folder routes through the
    // ported workspace.createFolder handler (same Y.Map writes + persistence).
    let mut durable_started = false;
    for folder in &plan.folders {
        let folder_operation = CrdtOperation {
            operation_id: format!("{operation_id}:{}", folder.folder_id),
            kind: "workspace.createFolder".to_string(),
            graph_id: graph_id.clone(),
            document_id: None,
            payload: json!({
                "folderId": folder.folder_id,
                "name": folder.name,
                "parentId": folder.parent_id,
                "section": "documents",
                "order": folder.order,
            }),
            enqueue_timestamp: enqueue_timestamp.clone(),
        };
        match super::workspace_ops::apply_classified(app, &folder_operation).await {
            Ok(_) => durable_started = true,
            Err(error) => return Err(promote_partial_import_error(error, durable_started)),
        }
    }

    import_result.tags_created = plan.tags_created;

    // ── per-document writes (this.writeDocument per assignment)
    for (index, assignment) in plan.assignments.iter().enumerate() {
        #[cfg(test)]
        if let Err(error) =
            maybe_fail_vault_step_for_test(&operation_id, VaultFailurePoint::BeforeDocument)
        {
            return Err(promote_partial_import_error(error, durable_started));
        }
        let write_operation = CrdtOperation {
            operation_id: format!("{operation_id}-{index}"),
            kind: "document.write".to_string(),
            graph_id: graph_id.clone(),
            document_id: Some(assignment.document_id.clone()),
            payload: json!({
                "documentId": assignment.document_id,
                "title": assignment.document.title,
                "content": assignment.document.markdown,
                "format": "markdown",
                "parentId": assignment.parent_id,
                "order": enqueue_ms + index as f64,
                "readOnly": false,
            }),
            enqueue_timestamp: enqueue_timestamp.clone(),
        };
        match super::document_ops::document_write_classified(app, &write_operation).await {
            Ok(_) => {
                durable_started = true;
                import_result.documents_created += 1;
                import_result
                    .document_ids
                    .push(assignment.document_id.clone());
            }
            Err(error) => return Err(promote_partial_import_error(error, durable_started)),
        }
    }

    // ── wikilink → wire creation
    for wire in &plan.wires {
        #[cfg(test)]
        if let Err(error) =
            maybe_fail_vault_step_for_test(&operation_id, VaultFailurePoint::BeforeWire)
        {
            return Err(promote_partial_import_error(error, durable_started));
        }
        let source_block_id = archive_source_block_id(
            app,
            &graph_id,
            &wire.source_document_id,
            &wire.block_candidates,
        );
        let wire_operation = CrdtOperation {
            operation_id: format!("{operation_id}:{}", wire.wire_id),
            kind: "workspace.createWire".to_string(),
            graph_id: graph_id.clone(),
            document_id: Some(wire.source_document_id.clone()),
            payload: json!({
                "wireId": wire.wire_id,
                "sourceDocumentId": wire.source_document_id,
                "sourceBlockId": source_block_id,
                "targetGraphId": graph_id,
                "targetDocumentId": wire.target_document_id,
                "predicate": wire.predicate,
                "sourceSnippet": wire.source_context,
                "updatedAt": enqueue_ms,
            }),
            enqueue_timestamp: enqueue_timestamp.clone(),
        };
        match super::workspace_ops::apply_classified(app, &wire_operation).await {
            Ok(_) => {
                durable_started = true;
                import_result.wires_created += 1;
            }
            Err(error) => return Err(promote_partial_import_error(error, durable_started)),
        }
    }

    import_result.unresolved_links = plan.unresolved.len();
    if !plan.unresolved.is_empty() {
        import_result
            .warnings
            .push(unresolved_links_warning(&plan.unresolved));
    }
    import_result.folders_created = plan.folders.len();
    // saveFilesystemWorkspace: each folder/document/wire op above already ran
    // the workspace persistence path (persist_workspace / save_document), so
    // no extra flush is needed here.
    let envelope = import_result_envelope(&import_result);

    // Append Tier B completion only after every deterministic step succeeds.
    // A ledger failure keeps the exact outer operation retryable; its stable
    // child IDs make the next attempt converge instead of duplicating work.
    finish_vault_completion(
        app,
        operation,
        &graph_id,
        pending_archive_path.as_deref(),
        envelope,
    )
}
