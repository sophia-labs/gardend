//! The keys Garden's Reader puts on a comment (2026-10-19).
//!
//! The Reader saves highlights and notes as entries of a document's
//! `comments` map. Beside the ordinary comment fields they carry four more
//! keys, and only these four are accepted from a caller of `edit_comment` /
//! `document.editComment(s)`; each is stored verbatim:
//!
//! - `kind`: `"comment"` | `"highlight"`. A highlight may have empty `text`.
//! - `color`: `"yellow"` | `"green"` | `"blue"` | `"pink"`.
//! - `anchor`: `{ page?: integer >= 1, chapter?: integer >= 0,
//!   prefix?: string <= 200 chars, suffix?: string <= 200 chars }`.
//! - `madeIn`: `"reader"`.
//!
//! Anything else under those names is refused with the reason. A key absent
//! from the payload leaves the stored entry's value alone.

use serde_json::{Map, Value};

pub(crate) const READER_KEYS: [&str; 4] = ["kind", "color", "anchor", "madeIn"];
const KINDS: [&str; 2] = ["comment", "highlight"];
const COLORS: [&str; 4] = ["yellow", "green", "blue", "pink"];
const MADE_IN: [&str; 1] = ["reader"];
const ANCHOR_KEYS: [&str; 4] = ["page", "chapter", "prefix", "suffix"];
const MAX_CONTEXT_CHARS: usize = 200;

fn one_of(key: &str, value: &Value, allowed: &[&str]) -> Result<(), String> {
    match value.as_str() {
        Some(text) if allowed.contains(&text) => Ok(()),
        _ => Err(format!(
            "{key} must be one of {} (got {value})",
            allowed
                .iter()
                .map(|a| format!("\"{a}\""))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn whole_number(value: &Value) -> Option<i64> {
    if let Some(n) = value.as_i64() {
        return Some(n);
    }
    // 3.0 is a whole number to a JS caller.
    value
        .as_f64()
        .filter(|f| f.fract() == 0.0 && f.abs() < 9.0e15)
        .map(|f| f as i64)
}

fn check_anchor(value: &Value) -> Result<(), String> {
    let Some(anchor) = value.as_object() else {
        return Err("anchor must be an object".to_string());
    };
    for (key, entry) in anchor {
        match key.as_str() {
            "page" => match whole_number(entry) {
                Some(n) if n >= 1 => {}
                _ => return Err(format!("anchor.page must be an integer >= 1 (got {entry})")),
            },
            "chapter" => match whole_number(entry) {
                Some(n) if n >= 0 => {}
                _ => {
                    return Err(format!(
                        "anchor.chapter must be an integer >= 0 (got {entry})"
                    ))
                }
            },
            "prefix" | "suffix" => match entry.as_str() {
                Some(text) if text.chars().count() <= MAX_CONTEXT_CHARS => {}
                _ => {
                    return Err(format!(
                        "anchor.{key} must be a string of at most {MAX_CONTEXT_CHARS} characters"
                    ))
                }
            },
            other => {
                return Err(format!(
                    "anchor.{other} is not allowed (anchor takes {})",
                    ANCHOR_KEYS.join(", ")
                ))
            }
        }
    }
    Ok(())
}

/// Validate the Reader keys a payload carries. Null counts as absent.
pub(crate) fn validate(payload: &Map<String, Value>) -> Result<(), String> {
    for key in READER_KEYS {
        let Some(value) = payload.get(key).filter(|v| !v.is_null()) else {
            continue;
        };
        match key {
            "kind" => one_of(key, value, &KINDS)?,
            "color" => one_of(key, value, &COLORS)?,
            "madeIn" => one_of(key, value, &MADE_IN)?,
            _ => check_anchor(value)?,
        }
    }
    Ok(())
}

/// Copy the payload's (validated) Reader keys onto a comment entry.
pub(crate) fn apply(data: &mut Map<String, Value>, payload: &Map<String, Value>) {
    for key in READER_KEYS {
        if let Some(value) = payload.get(key).filter(|v| !v.is_null()) {
            data.insert(key.to_string(), value.clone());
        }
    }
}

/// Whether the entry being written is a highlight (payload first, else stored).
pub(crate) fn is_highlight(payload: &Map<String, Value>, stored: &Map<String, Value>) -> bool {
    payload
        .get("kind")
        .filter(|v| !v.is_null())
        .or_else(|| stored.get("kind"))
        .and_then(Value::as_str)
        == Some("highlight")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn check(value: Value) -> Result<(), String> {
        validate(value.as_object().unwrap())
    }

    #[test]
    fn the_allowlist_is_accepted() {
        check(json!({"kind":"highlight","color":"pink","madeIn":"reader",
            "anchor":{"page":1,"chapter":0,"prefix":"a","suffix":"b"}}))
        .unwrap();
        check(json!({"kind":"comment","color":"yellow","anchor":{}})).unwrap();
        check(json!({"text":"nothing reader-ish","kind":null})).unwrap();
    }

    #[test]
    fn everything_else_is_refused_with_a_reason() {
        for (payload, needle) in [
            (json!({"kind":"note"}), "kind must be one of"),
            (json!({"color":"red"}), "color must be one of"),
            (json!({"madeIn":"web"}), "madeIn must be one of"),
            (json!({"anchor":"p3"}), "anchor must be an object"),
            (json!({"anchor":{"page":0}}), "anchor.page"),
            (json!({"anchor":{"page":1.5}}), "anchor.page"),
            (json!({"anchor":{"chapter":-1}}), "anchor.chapter"),
            (
                json!({"anchor":{"prefix":"x".repeat(201)}}),
                "anchor.prefix",
            ),
            (json!({"anchor":{"suffix":7}}), "anchor.suffix"),
            (
                json!({"anchor":{"cfi":"epubcfi(/6/4)"}}),
                "anchor.cfi is not allowed",
            ),
        ] {
            let error = check(payload.clone()).expect_err(&payload.to_string());
            assert!(error.contains(needle), "{error} should mention {needle}");
        }
        check(json!({"anchor":{"prefix":"é".repeat(200)}})).expect("200 characters is the limit");
    }
}
