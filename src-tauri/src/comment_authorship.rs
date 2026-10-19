//! Who wrote a comment, as gardend authenticated them (2026-10-14).
//!
//! A comment's `author` used to be whatever the caller said: an `edit_comment`
//! (or a `write_document` carrying `comments`) signed "Vera (owner)" was
//! stored as Vera's (W6 probe F5). Now the server signs it:
//!
//! - At enqueue, in the request's own task where the principal is known, every
//!   `document.editComment`, and every `document.write` that carries comments,
//!   gets the principal under [`COMMENT_AUTHOR_PAYLOAD_KEY`]. A copy the caller
//!   sent is dropped first. The payload is journaled, so a recovered operation
//!   keeps the author it had.
//! - The executor writes it into the comment ([`sign_comment`]):
//!   - `authorId`: the principal. `owner` (the master key), `token:<id>`, an
//!     Access email, `service:<id>`, `access-service:<id>`, or a hosted cell's
//!     lease principal (`user:<sub>`; W1's `cognito:<sub>` later). A
//!     delegated agent writes as the person it acts for.
//!   - `author`: its display name. `Owner`, the client token's label, the
//!     email, or the principal itself, plus ` via <service>` for a delegated
//!     agent. This is what every UI shows.
//!   - `authorVia`: `service:<id>` for a delegated agent, absent otherwise.
//!   - `claimedAuthor`: what the caller passed as `author`, kept only when it
//!     differs. It is unverified and never shown as the author.
//!
//! Comments written straight into a document room (the SPA's editor, the
//! Neovim bridge) do not pass through here; see the integrity receipt.

use crate::app_runtime::AppHandle;
use serde_json::{json, Map, Value};
use std::sync::Arc;
#[cfg(feature = "desktop")]
use tauri::Manager;

/// Server-only payload key; stripped from every caller payload at enqueue.
pub(crate) const COMMENT_AUTHOR_PAYLOAD_KEY: &str = "__gardenCommentAuthor";

/// Who an operation's comments are written by when no request named anyone
/// (an internal write); the same name the audit log uses.
const RUNTIME_PRINCIPAL: &str = "local-runtime";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommentAuthor {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) via: Option<String>,
}

impl CommentAuthor {
    fn runtime() -> Self {
        Self {
            id: RUNTIME_PRINCIPAL.to_string(),
            name: RUNTIME_PRINCIPAL.to_string(),
            via: None,
        }
    }

    fn to_json(&self) -> Value {
        json!({ "id": self.id, "name": self.name, "via": self.via })
    }

    /// The author the enqueue stamped into this payload. An operation that
    /// did not come through the enqueue (or was journaled before 2026-10-14)
    /// has none, and is signed by the runtime: never by the caller.
    pub(crate) fn from_payload(payload: &Map<String, Value>) -> Self {
        let Some(stamp) = payload.get(COMMENT_AUTHOR_PAYLOAD_KEY) else {
            return Self::runtime();
        };
        let text = |key: &str| {
            stamp
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };
        match (text("id"), text("name")) {
            (Some(id), Some(name)) => Self {
                id,
                name,
                via: text("via"),
            },
            _ => Self::runtime(),
        }
    }
}

/// The principal of the request this task serves, as a comment author.
pub(crate) fn current_comment_author(app: &AppHandle) -> CommentAuthor {
    // W6: a visitor signs with its account id and presence name, never its
    // Cognito subject or its email.
    if let Some(fence) = crate::visitor_fence::current_fence() {
        let via = crate::request_actor::current_request_actor().and_then(|actor| actor.via);
        let name = match via.as_deref() {
            Some(via) => format!(
                "{} via {}",
                fence.name,
                via.strip_prefix("service:").unwrap_or(via)
            ),
            None => fence.name.clone(),
        };
        return CommentAuthor {
            id: fence.account,
            name,
            via,
        };
    }
    if let Some(actor) = crate::request_actor::current_request_actor() {
        let mut name = display_name(app, &actor.user_id);
        if let Some(via) = actor.via.as_deref() {
            name = format!("{name} via {}", via.strip_prefix("service:").unwrap_or(via));
        }
        return CommentAuthor {
            id: actor.user_id,
            name,
            via: actor.via,
        };
    }
    if let Some(lease) = crate::cell_graph_boundary::current_cell_lease() {
        let owner = app
            .try_state::<Arc<crate::cell_graph_boundary::CellGraphBoundary>>()
            .and_then(|boundary| boundary.owner_principal().map(str::to_string));
        let name = if owner.as_deref() == Some(lease.principal.as_str()) {
            "Owner".to_string()
        } else {
            lease.principal.clone()
        };
        return CommentAuthor {
            id: lease.principal,
            name,
            via: None,
        };
    }
    CommentAuthor::runtime()
}

fn display_name(app: &AppHandle, user_id: &str) -> String {
    if user_id == "owner" {
        return "Owner".to_string();
    }
    if let Some(token_id) = user_id.strip_prefix("token:") {
        let label = crate::loopback_client_token_service::list_client_tokens(app)
            .ok()
            .and_then(|tokens| {
                tokens
                    .into_iter()
                    .find(|token| token.token_id == token_id)
                    .map(|token| token.label.trim().to_string())
            })
            .filter(|label| !label.is_empty());
        if let Some(label) = label {
            return label;
        }
    }
    user_id.to_string()
}

/// At enqueue: drop any caller copy of the server-only key, then stamp the
/// current principal into the payloads that write comments.
pub(crate) fn stamp_comment_author(app: &AppHandle, kind: &str, payload: &mut Value) {
    let Some(object) = payload.as_object_mut() else {
        return;
    };
    object.remove(COMMENT_AUTHOR_PAYLOAD_KEY);
    let writes_comments = match kind {
        "document.editComment" => true,
        "document.write" => object
            .get("comments")
            .and_then(Value::as_object)
            .is_some_and(|comments| !comments.is_empty()),
        _ => false,
    };
    if writes_comments {
        object.insert(
            COMMENT_AUTHOR_PAYLOAD_KEY.to_string(),
            current_comment_author(app).to_json(),
        );
    }
}

/// The authorship fields of a stored comment.
const AUTHORSHIP_FIELDS: [&str; 5] = [
    "author",
    "authorId",
    "author_id",
    "authorVia",
    "claimedAuthor",
];

/// Sign `data` (a comment about to be stored) by `author`. Whatever the
/// caller put in the authorship fields is dropped; the name it claimed
/// (`claimed`, its `author` argument), when it differs from the signed name,
/// is kept as `claimedAuthor`.
///
/// `existing` is the comment as stored now, if any. When the text does not
/// change (a resolve, a re-anchor, a `write_document` that hands back the
/// comments it read), the stored authorship is kept: the author of a comment
/// is whoever last wrote its text.
pub(crate) fn sign_comment(
    data: &mut Map<String, Value>,
    author: &CommentAuthor,
    claimed: Option<&Value>,
    existing: Option<&Map<String, Value>>,
) {
    let claimed = claimed
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|claimed| !claimed.is_empty())
        .map(str::to_string);
    for field in AUTHORSHIP_FIELDS {
        data.remove(field);
    }
    if let Some(existing) = existing.filter(|existing| {
        existing.get("authorId").is_some() && existing.get("text") == data.get("text")
    }) {
        for field in AUTHORSHIP_FIELDS {
            if let Some(value) = existing.get(field) {
                data.insert(field.to_string(), value.clone());
            }
        }
        return;
    }
    data.insert("author".to_string(), json!(author.name));
    data.insert("authorId".to_string(), json!(author.id));
    if let Some(via) = &author.via {
        data.insert("authorVia".to_string(), json!(via));
    }
    if let Some(claimed) = claimed.filter(|claimed| claimed != &author.name) {
        data.insert("claimedAuthor".to_string(), json!(claimed));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn author() -> CommentAuthor {
        CommentAuthor {
            id: "token:t-1".into(),
            name: "Lucy's laptop via vehicle".into(),
            via: Some("service:vehicle".into()),
        }
    }

    #[test]
    fn a_forged_author_becomes_a_claim_and_the_principal_signs() {
        let mut data = json!({
            "text": "hello",
            "author": "Vera (owner)",
            "authorId": "owner",
            "author_id": "owner",
            "authorVia": "service:nobody",
        })
        .as_object()
        .cloned()
        .unwrap();
        let claimed = data.get("author").cloned();
        sign_comment(&mut data, &author(), claimed.as_ref(), None);
        assert_eq!(data["author"], "Lucy's laptop via vehicle");
        assert_eq!(data["authorId"], "token:t-1");
        assert_eq!(data["authorVia"], "service:vehicle");
        assert_eq!(data["claimedAuthor"], "Vera (owner)");
        assert!(data.get("author_id").is_none());
    }

    #[test]
    fn unchanged_text_keeps_the_stored_author_and_new_text_takes_the_writer() {
        let stored = json!({ "text": "hello", "author": "Owner", "authorId": "owner" })
            .as_object()
            .cloned()
            .unwrap();
        let mut same = json!({ "text": "hello", "author": "Mallory" })
            .as_object()
            .cloned()
            .unwrap();
        let claimed = same.get("author").cloned();
        sign_comment(&mut same, &author(), claimed.as_ref(), Some(&stored));
        assert_eq!(same["authorId"], "owner");
        assert_eq!(same["author"], "Owner");
        assert!(same.get("claimedAuthor").is_none());

        let mut changed = json!({ "text": "rewritten" }).as_object().cloned().unwrap();
        sign_comment(&mut changed, &author(), None, Some(&stored));
        assert_eq!(changed["authorId"], "token:t-1");
        assert!(changed.get("claimedAuthor").is_none());
    }

    #[test]
    fn a_payload_without_the_server_stamp_is_signed_by_the_runtime() {
        let payload = json!({ "author": "Vera (owner)" })
            .as_object()
            .cloned()
            .unwrap();
        assert_eq!(
            CommentAuthor::from_payload(&payload),
            CommentAuthor::runtime()
        );
    }
}
