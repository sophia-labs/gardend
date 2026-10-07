//! Closed validation and logical identity for the built-in Garden Files pack.
//!
//! This validates the record fields and the request graph. Authentication of
//! ownerId and comparison of graphIncarnation with the active ledger belong to
//! the trusted route/source-ledger caller.

use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

pub(crate) const VIEW_NAMESPACE: &str = "https://sophia-labs.ai/ontology/view#";
pub(crate) const FILE_VIEWS_WRITE_TARGET: &str = "projection:file-views";
pub(crate) const FOLDER_VIEW_CLASS: &str = "FolderView";
pub(crate) const FILE_PLACEMENT_CLASS: &str = "FilePlacement";
pub(crate) const PRESENTATION_VALUES: &[&str] = &["canvas", "list"];
pub(crate) const SORT_VALUES: &[&str] = &["manual", "name", "type", "modified"];
pub(crate) const DIRECTION_VALUES: &[&str] = &["asc", "desc"];
pub(crate) const ICON_SIZE_VALUES: &[&str] = &["small", "medium", "large"];
pub(crate) const AUDIENCE_KIND_VALUES: &[&str] = &["shared", "personal"];
pub(crate) const FILE_KIND_VALUES: &[&str] = &["folder", "document", "artifact"];
pub(crate) const COORDINATE_SPACE_VALUES: &[&str] = &["folder-canvas"];

const FOLDER_VIEW_FIELDS: &[&str] = &[
    "kind",
    "localId",
    "schemaVersion",
    "ownerId",
    "graphId",
    "graphIncarnation",
    "folderKey",
    "viewId",
    "audienceKind",
    "audienceUserId",
    "presentation",
    "sort",
    "direction",
    "iconSize",
    "snapToGrid",
];
const FILE_PLACEMENT_FIELDS: &[&str] = &[
    "kind",
    "localId",
    "schemaVersion",
    "ownerId",
    "graphId",
    "graphIncarnation",
    "folderKey",
    "viewId",
    "audienceKind",
    "audienceUserId",
    "fileKind",
    "fileId",
    "coordinateSpace",
    "x",
    "y",
];

/// Validate one flat FolderView or FilePlacement generic object before it is
/// admitted to source authority. `expected_graph_id` must come from the routed
/// request, not from the object itself.
pub(crate) fn validate_folder_view_record(
    value: &Value,
    expected_graph_id: &str,
) -> Result<(), String> {
    let object = value
        .as_object()
        .ok_or("folder-view record must be an object")?;
    let kind = required_string(object, "kind")?;
    let expected_fields = match kind {
        FOLDER_VIEW_CLASS => FOLDER_VIEW_FIELDS,
        FILE_PLACEMENT_CLASS => FILE_PLACEMENT_FIELDS,
        _ => return Err("kind must be FolderView or FilePlacement".into()),
    };
    let allowed: BTreeSet<&str> = expected_fields.iter().copied().collect();
    if object.keys().any(|key| !allowed.contains(key.as_str())) {
        return Err("folder-view record contains an unknown field".into());
    }

    if required_string(object, "localId")?.is_empty() {
        return Err("localId must not be empty".into());
    }
    if object.get("schemaVersion").and_then(Value::as_i64) != Some(1) {
        return Err("schemaVersion must be integer 1".into());
    }
    for field in ["ownerId", "graphId", "graphIncarnation", "viewId"] {
        if required_string(object, field)?.trim().is_empty() {
            return Err(format!("{field} must not be empty"));
        }
    }
    if required_string(object, "graphId")? != expected_graph_id {
        return Err("graphId does not match the routed request".into());
    }
    let folder_key = required_string(object, "folderKey")?;
    if folder_key != "root"
        && !folder_key
            .strip_prefix("folder:")
            .is_some_and(|raw_id| !raw_id.trim().is_empty())
    {
        return Err("folderKey must be root or folder:<stable-id>".into());
    }
    validate_audience(object)?;

    match kind {
        FOLDER_VIEW_CLASS => {
            one_of(object, "presentation", PRESENTATION_VALUES)?;
            one_of(object, "sort", SORT_VALUES)?;
            one_of(object, "direction", DIRECTION_VALUES)?;
            one_of(object, "iconSize", ICON_SIZE_VALUES)?;
            if object.get("snapToGrid").and_then(Value::as_bool).is_none() {
                return Err("snapToGrid must be a boolean".into());
            }
        }
        FILE_PLACEMENT_CLASS => {
            one_of(object, "fileKind", FILE_KIND_VALUES)?;
            if required_string(object, "fileId")?.trim().is_empty() {
                return Err("fileId must not be empty".into());
            }
            if !COORDINATE_SPACE_VALUES.contains(&required_string(object, "coordinateSpace")?) {
                return Err("coordinateSpace must be folder-canvas".into());
            }
            for field in ["x", "y"] {
                if !object
                    .get(field)
                    .and_then(Value::as_f64)
                    .is_some_and(f64::is_finite)
                {
                    return Err(format!("{field} must be a finite number"));
                }
            }
        }
        _ => unreachable!("kind was matched above"),
    }

    let expected_id = folder_view_local_id(value)?;
    if required_string(object, "localId")? != expected_id {
        return Err("localId does not match the canonical scope identity".into());
    }
    Ok(())
}

/// Canonical localId: SHA-256 over the UTF-8 bytes of the TS qualified key.
/// Collision resistance is assumed; this is not an injectivity proof.
pub(crate) fn folder_view_local_id(value: &Value) -> Result<String, String> {
    let object = value
        .as_object()
        .ok_or("folder-view record must be an object")?;
    let kind = required_string(object, "kind")?;
    let folder_key = required_string(object, "folderKey")?;
    let audience_kind = required_string(object, "audienceKind")?;
    let audience = match audience_kind {
        "shared" => "shared".to_owned(),
        "personal" => format!("personal:{}", required_string(object, "audienceUserId")?),
        _ => return Err("audienceKind must be shared or personal".into()),
    };
    let mut parts = vec![
        required_string(object, "ownerId")?.to_owned(),
        required_string(object, "graphId")?.to_owned(),
        required_string(object, "graphIncarnation")?.to_owned(),
        folder_key.to_owned(),
        required_string(object, "viewId")?.to_owned(),
        audience,
    ];
    let prefix = match kind {
        FOLDER_VIEW_CLASS => "fv-",
        FILE_PLACEMENT_CLASS => {
            parts.push(required_string(object, "fileKind")?.to_owned());
            parts.push(required_string(object, "fileId")?.to_owned());
            "fp-"
        }
        _ => return Err("kind must be FolderView or FilePlacement".into()),
    };
    let key = parts
        .iter()
        .map(|part| encode_uri_component(part))
        .collect::<Vec<_>>()
        .join("/");
    let digest = Sha256::digest(key.as_bytes());
    Ok(format!("{prefix}{digest:x}"))
}

fn validate_audience(object: &serde_json::Map<String, Value>) -> Result<(), String> {
    one_of(object, "audienceKind", AUDIENCE_KIND_VALUES)?;
    match required_string(object, "audienceKind")? {
        "shared" if !object.contains_key("audienceUserId") => Ok(()),
        "shared" => Err("audienceUserId is forbidden for shared preferences".into()),
        "personal" => {
            if required_string(object, "audienceUserId")?.trim().is_empty() {
                return Err("audienceUserId must not be empty for personal preferences".into());
            }
            Ok(())
        }
        _ => Err("audienceKind must be shared or personal".into()),
    }
}

fn required_string<'a>(
    object: &'a serde_json::Map<String, Value>,
    field: &str,
) -> Result<&'a str, String> {
    object
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{field} must be a string"))
}

fn one_of(
    object: &serde_json::Map<String, Value>,
    field: &str,
    values: &[&str],
) -> Result<(), String> {
    let value = required_string(object, field)?;
    if values.contains(&value) {
        Ok(())
    } else {
        Err(format!("{field} has an unsupported value"))
    }
}

/// JavaScript encodeURIComponent's unescaped ASCII set; all other UTF-8 bytes
/// use uppercase percent escapes, matching `folderViewKey` exactly.
fn encode_uri_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            encoded.push(byte as char);
        } else {
            use std::fmt::Write as _;
            write!(&mut encoded, "%{byte:02X}").expect("writing to String cannot fail");
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn root_view() -> Value {
        json!({
            "kind":"FolderView", "localId":"fv-6af78db18c6b455722e9193457bec0ca46dfdb5afed6e8d6905109defb342673",
            "schemaVersion":1, "ownerId":"owner", "graphId":"graph", "graphIncarnation":"inc",
            "folderKey":"root", "viewId":"files-default", "audienceKind":"shared",
            "presentation":"canvas", "sort":"manual", "direction":"asc", "iconSize":"medium",
            "snapToGrid":true
        })
    }

    fn unicode_personal_placement(kind: &str) -> Value {
        let local_id = match kind {
            "document" => "fp-04526a84ce21b47f194d4dbe453361f6b5f784e889f8e8f379bee27b03c963f4",
            "artifact" => "fp-191b17854a4dd413ccfa2a3d9f69fb57fda636aa67b0b485acc28dd49e495732",
            _ => unreachable!(),
        };
        json!({
            "kind":"FilePlacement", "localId":local_id, "schemaVersion":1,
            "ownerId":"Owner/Team", "graphId":"graph%2Fone", "graphIncarnation":"inc✓",
            "folderKey":"folder:power/supply %", "viewId":"files/default", "audienceKind":"personal",
            "audienceUserId":"Ada / 李", "fileKind":kind, "fileId":"manual/100% 🧾",
            "coordinateSpace":"folder-canvas", "x":12.5, "y":-4.0
        })
    }

    #[test]
    fn canonical_identity_vectors_match_typescript_key_encoding() {
        assert_eq!(VIEW_NAMESPACE, "https://sophia-labs.ai/ontology/view#");
        assert_eq!(FILE_VIEWS_WRITE_TARGET, "projection:file-views");
        let root = root_view();
        assert_eq!(
            folder_view_local_id(&root).unwrap(),
            "fv-6af78db18c6b455722e9193457bec0ca46dfdb5afed6e8d6905109defb342673"
        );
        assert_eq!(
            folder_view_local_id(&unicode_personal_placement("document")).unwrap(),
            "fp-04526a84ce21b47f194d4dbe453361f6b5f784e889f8e8f379bee27b03c963f4"
        );
        assert_eq!(
            folder_view_local_id(&unicode_personal_placement("artifact")).unwrap(),
            "fp-191b17854a4dd413ccfa2a3d9f69fb57fda636aa67b0b485acc28dd49e495732"
        );
        assert_ne!(
            folder_view_local_id(&unicode_personal_placement("document")).unwrap(),
            folder_view_local_id(&unicode_personal_placement("artifact")).unwrap(),
            "document and artifact IDs with the same raw text have distinct identities"
        );
        let mut child = root.clone();
        child["folderKey"] = Value::String("folder:root".into());
        child["localId"] = Value::String(folder_view_local_id(&child).unwrap());
        assert_ne!(
            root["localId"], child["localId"],
            "root does not alias a folder ID"
        );
    }

    #[test]
    fn validator_accepts_canonical_examples_and_rejects_bad_fields() {
        assert!(validate_folder_view_record(&root_view(), "graph").is_ok());
        let placement = unicode_personal_placement("document");
        assert!(validate_folder_view_record(&placement, "graph%2Fone").is_ok());

        let mut bad = placement.clone();
        bad["graphId"] = Value::String("other".into());
        assert!(validate_folder_view_record(&bad, "graph%2Fone").is_err());
        let mut bad = placement.clone();
        bad["x"] = Value::String("12.5".into());
        assert!(validate_folder_view_record(&bad, "graph%2Fone").is_err());
        let mut bad = placement.clone();
        bad["coordinateSpace"] = Value::String("pdf-user-space".into());
        assert!(validate_folder_view_record(&bad, "graph%2Fone").is_err());
        let mut bad = placement.clone();
        bad["fileKind"] = Value::String("image".into());
        assert!(validate_folder_view_record(&bad, "graph%2Fone").is_err());
        let mut bad = placement.clone();
        bad["audienceUserId"] = Value::Null;
        assert!(validate_folder_view_record(&bad, "graph%2Fone").is_err());
        let mut bad = placement.clone();
        bad["schemaVersion"] = Value::from(2);
        assert!(validate_folder_view_record(&bad, "graph%2Fone").is_err());
        let mut bad = placement.clone();
        bad["extra"] = Value::Bool(true);
        assert!(validate_folder_view_record(&bad, "graph%2Fone").is_err());
        let mut bad = placement;
        bad["localId"] = Value::String("fp-wrong".into());
        assert!(validate_folder_view_record(&bad, "graph%2Fone").is_err());
    }
}
