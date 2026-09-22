//! PDF anchor v1 wire inspection. Raw document attributes are never rewritten.
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

pub(crate) const ATTRIBUTE: &str = "data-pdf-anchor";
pub(crate) const MAX_ANCHOR_BYTES: usize = 65_536;
pub(crate) const MAX_TARGETS: usize = 64;
pub(crate) const MAX_RECTS: usize = 2_048;
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Anchor {
    pub(crate) version: f64,
    pub(crate) text_sha256: String,
    pub(crate) source: Source,
    pub(crate) relation: String,
    pub(crate) precision: String,
    pub(crate) extraction: Extraction,
    pub(crate) targets: Vec<Target>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Source {
    pub(crate) sha256: String,
    pub(crate) byte_length: f64,
    pub(crate) artifact_id: Option<String>,
    pub(crate) revision_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Extraction {
    pub(crate) approach: String,
    pub(crate) version: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Target {
    pub(crate) page_index: f64,
    pub(crate) space: String,
    pub(crate) view_box: [f64; 4],
    pub(crate) user_unit: f64,
    pub(crate) rotation: f64,
    pub(crate) rects: Vec<[f64; 4]>,
}

#[derive(Debug)]
pub(crate) enum Inspection {
    Absent,
    Invalid(&'static str),
    Unsupported(&'static str),
    Mapped(Anchor),
}

pub(crate) fn sha256(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(bytes.as_ref()))
}

pub(crate) fn is_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub(crate) fn is_locator(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn safe_integer(value: f64, minimum: f64) -> bool {
    value.is_finite() && value >= minimum && value <= MAX_SAFE_INTEGER && value.fract() == 0.0
}

fn rect_valid(rect: &[f64; 4], unit: f64) -> bool {
    if !rect.iter().all(|n| n.is_finite() && n.abs() <= 1e9) {
        return false;
    }
    let width = rect[2] - rect[0];
    let height = rect[3] - rect[1];
    [
        width,
        height,
        width * height,
        width * unit,
        height * unit,
        (width * unit) * (height * unit),
    ]
    .iter()
    .all(|n| n.is_finite() && *n > 0.0)
}

pub(crate) fn inspect(value: Option<&Value>) -> Inspection {
    let Some(value) = value.filter(|v| !v.is_null()) else {
        return Inspection::Absent;
    };
    let Some(raw) = value.as_str() else {
        return Inspection::Invalid("anchor must be a JSON string");
    };
    if raw.len() > MAX_ANCHOR_BYTES {
        return Inspection::Invalid("anchor exceeds 65536 UTF-8 bytes");
    }
    let Ok(parsed) = serde_json::from_str::<Value>(raw) else {
        return Inspection::Invalid("anchor is not valid JSON");
    };
    let Some(version) = parsed.get("version").and_then(Value::as_f64) else {
        return Inspection::Invalid("anchor version must be a number");
    };
    if safe_integer(version, 2.0) {
        return Inspection::Unsupported("unsupported PDF anchor version");
    }
    if version != 1.0 {
        return Inspection::Invalid("anchor version must be 1 or a safe future integer");
    }
    // Option<T> would accept explicit null; the wire contract permits absence,
    // not a null locator. Preserve it but classify it as invalid.
    if let Some(source) = parsed.get("source") {
        for key in ["artifactId", "revisionId"] {
            if source.get(key).is_some_and(Value::is_null) {
                return Inspection::Invalid("locator IDs must be strings when present");
            }
        }
    }
    let Ok(anchor) = serde_json::from_value::<Anchor>(parsed) else {
        return Inspection::Invalid("anchor has missing, unknown or wrongly typed fields");
    };
    if !is_hash(&anchor.text_sha256)
        || !is_hash(&anchor.source.sha256)
        || !safe_integer(anchor.source.byte_length, 1.0)
        || anchor.relation != "derived-from"
        || anchor.precision != "block"
    {
        return Inspection::Invalid("invalid hash, byte length or relationship precision");
    }
    if anchor
        .source
        .artifact_id
        .as_deref()
        .is_some_and(|s| !is_locator(s))
        || anchor
            .source
            .revision_id
            .as_deref()
            .is_some_and(|s| !is_locator(s))
        || (anchor.source.revision_id.is_some() && anchor.source.artifact_id.is_none())
    {
        return Inspection::Invalid("invalid graph-local artifact revision locator");
    }
    if [&anchor.extraction.approach, &anchor.extraction.version]
        .iter()
        .any(|s| s.trim().is_empty() || s.len() > 128)
    {
        return Inspection::Invalid(
            "extraction labels must be nonempty and at most 128 UTF-8 bytes",
        );
    }
    if anchor.targets.is_empty() || anchor.targets.len() > MAX_TARGETS {
        return Inspection::Invalid("anchor requires 1 to 64 page targets");
    }
    let mut count = 0usize;
    for target in &anchor.targets {
        if !safe_integer(target.page_index, 0.0)
            || target.space != "pdf-user-space"
            || ![0.0, 90.0, 180.0, 270.0].contains(&target.rotation)
            || !target.user_unit.is_finite()
            || target.user_unit <= 0.0
            || target.user_unit > 1e6
            || !rect_valid(&target.view_box, target.user_unit)
            || target.rects.is_empty()
        {
            return Inspection::Invalid("invalid page index, coordinate frame or empty rectangles");
        }
        count = count.saturating_add(target.rects.len());
        if count > MAX_RECTS {
            return Inspection::Invalid("anchor exceeds 2048 rectangles");
        }
        for rect in &target.rects {
            if !rect_valid(rect, target.user_unit)
                || rect[0] < target.view_box[0]
                || rect[1] < target.view_box[1]
                || rect[2] > target.view_box[2]
                || rect[3] > target.view_box[3]
            {
                return Inspection::Invalid(
                    "rectangle is empty, nonfinite or outside the page crop",
                );
            }
        }
    }
    Inspection::Mapped(anchor)
}

/// Hash exact descendant text leaves once, in document order. No separators,
/// cached ancestor text, marks or non-text leaves. Iterative, no recursive stack.
pub(crate) fn text_sha256(block: &Value) -> String {
    let mut hasher = Sha256::new();
    let mut stack = vec![block];
    while let Some(node) = stack.pop() {
        let children = node.get("content").and_then(Value::as_array);
        if node.get("type").and_then(Value::as_str) == Some("text")
            && children.is_none_or(Vec::is_empty)
        {
            if let Some(text) = node.get("text").and_then(Value::as_str) {
                hasher.update(text.as_bytes());
            }
        } else if let Some(children) = children {
            stack.extend(children.iter().rev());
        }
    }
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn fixture() -> Value {
        serde_json::from_str::<Value>(include_str!("pdf_source_fixture.json")).unwrap()["anchor"]
            .clone()
    }
    fn inspect_json(value: &Value) -> Inspection {
        inspect(Some(&Value::String(value.to_string())))
    }
    #[test]
    fn portable_value_and_exact_text_digest() {
        assert!(matches!(inspect_json(&fixture()), Inspection::Mapped(_)));
        let block = json!({"type":"paragraph","text":"not counted","content":[
            {"type":"text","text":"Aé","marks":[{"type":"bold"}]},
            {"type":"hardBreak"},{"type":"span","content":[{"type":"text","text":"\nB"}]}
        ]});
        assert_eq!(
            text_sha256(&block),
            fixture()["textSha256"].as_str().unwrap()
        );
    }
    #[test]
    fn malformed_future_null_and_numerical_limits_are_distinct() {
        for version in [0.0, -1.0, 1.5, 9_007_199_254_740_992.0] {
            let mut f = fixture();
            f["version"] = json!(version);
            assert!(matches!(inspect_json(&f), Inspection::Invalid(_)));
        }
        assert_eq!(
            text_sha256(
                &json!({"type":"text","text":"cached", "content":[{"type":"text","text":"real"}]})
            ),
            sha256("real")
        );
        assert!(matches!(inspect(None), Inspection::Absent));
        assert!(matches!(inspect(Some(&Value::Null)), Inspection::Absent));
        assert!(matches!(inspect(Some(&json!({}))), Inspection::Invalid(_)));
        let mut f = fixture();
        f["version"] = json!(2);
        assert!(matches!(inspect_json(&f), Inspection::Unsupported(_)));
        for mutation in [
            "extra", "null", "page", "unit", "outside", "textHash", "empty",
        ] {
            let mut f = fixture();
            match mutation {
                "extra" => f["surprise"] = json!(true),
                "null" => f["source"]["artifactId"] = Value::Null,
                "page" => f["targets"][0]["pageIndex"] = json!(0.5),
                "unit" => f["targets"][0]["userUnit"] = json!(1e100),
                "outside" => f["targets"][0]["rects"][0][0] = json!(-11),
                "textHash" => f["textSha256"] = json!("ABC"),
                "empty" => f["targets"] = json!([]),
                _ => unreachable!(),
            }
            assert!(
                matches!(inspect_json(&f), Inspection::Invalid(_)),
                "{mutation}"
            );
        }
    }
}
