//! Signed continuation cursors for paging an external SPARQL SELECT past the
//! row ceiling (`GARDEN_SPARQL_MAX_ROWS`, or a public visitor's ceiling).
//!
//! A truncated SELECT answer carries `next`: `sq1.<payload>.<mac>`, where the
//! payload (base64url JSON) names the graph, a fingerprint of the query text
//! and the dataset the caller sees, the offset of the next page, the page
//! size, and the store revision the answer was read at; the mac is
//! HMAC-SHA256 over the payload under a key drawn at process start. The
//! caller sends the SAME query with `cursor` to fetch the next page.
//!
//! - A cursor whose mac does not verify (edited, made up, or issued by an
//!   earlier run of this cell) is refused as invalid.
//! - A cursor for another graph, another query text, or another dataset (a
//!   public visitor does not see the Trash) is refused as from another query.
//! - A cursor issued before a write to the graph store is refused as stale
//!   (checked by the caller against [`crate::rdf_store_service::graph_store_revision`]).
//!
//! Pages are evaluated with the query as written, skipping `offset`
//! solutions. Without an `ORDER BY`, pages follow Oxigraph's evaluation
//! order, which is deterministic for an unchanged store (index scans are
//! key-ordered and the evaluator's hash tables use a fixed hasher). The server
//! never injects an `ORDER BY`: sorting materializes the whole result inside
//! Oxigraph on every page, the memory hazard the row ceiling exists to stop.
use crate::app_error::{AppError, AppResult};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::OnceLock;

const PREFIX: &str = "sq1";

#[derive(Debug, Serialize, Deserialize)]
struct Payload {
    /// Graph id.
    g: String,
    /// Query fingerprint ([`query_fingerprint`]).
    f: String,
    /// Offset of the page this cursor fetches.
    o: usize,
    /// Page size the cursor was issued with.
    n: usize,
    /// Store revision the issuing page was read at.
    r: String,
}

/// A verified cursor: where the next page starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PageStart {
    pub(crate) offset: usize,
    pub(crate) page_size: usize,
    pub(crate) revision: String,
}

fn key() -> &'static [u8; 32] {
    static KEY: OnceLock<[u8; 32]> = OnceLock::new();
    KEY.get_or_init(|| {
        let mut key = [0u8; 32];
        key[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        key[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        key
    })
}

fn mac(payload: &str) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key()).expect("HMAC accepts any key length");
    mac.update(PREFIX.as_bytes());
    mac.update(b".");
    mac.update(payload.as_bytes());
    mac
}

/// The query as this caller runs it: graph, exact query text, and the named
/// graphs hidden from the caller's dataset.
pub(crate) fn query_fingerprint(graph_id: &str, query: &str, hidden_graphs: &[String]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(graph_id.as_bytes());
    hasher.update([0]);
    hasher.update(query.as_bytes());
    for hidden in hidden_graphs {
        hasher.update([0]);
        hasher.update(hidden.as_bytes());
    }
    hasher.finalize()[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) fn mint(
    graph_id: &str,
    fingerprint: &str,
    offset: usize,
    page_size: usize,
    revision: &str,
) -> String {
    let payload = Payload {
        g: graph_id.to_string(),
        f: fingerprint.to_string(),
        o: offset,
        n: page_size,
        r: revision.to_string(),
    };
    let payload =
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).expect("cursor payload serializes"));
    let tag = URL_SAFE_NO_PAD.encode(mac(&payload).finalize().into_bytes());
    format!("{PREFIX}.{payload}.{tag}")
}

/// Verify `cursor` for this graph and query. The revision is checked by the
/// caller against the open store.
pub(crate) fn open(cursor: &str, graph_id: &str, fingerprint: &str) -> AppResult<PageStart> {
    let invalid = || {
        AppError::validation(
            "invalid SPARQL cursor: it does not verify (edited, made up, or issued before this \
             cell restarted); re-run the query without `cursor`",
        )
    };
    let mut parts = cursor.trim().split('.');
    let (Some(PREFIX), Some(payload), Some(tag), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(invalid());
    };
    let tag = URL_SAFE_NO_PAD.decode(tag).map_err(|_| invalid())?;
    mac(payload).verify_slice(&tag).map_err(|_| invalid())?;
    let payload: Payload = URL_SAFE_NO_PAD
        .decode(payload)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .ok_or_else(invalid)?;
    if payload.g != graph_id || payload.f != fingerprint {
        return Err(AppError::validation(
            "SPARQL cursor belongs to another query: send the exact query text (and graph) the \
             cursor was issued for, or re-run without `cursor`",
        ));
    }
    Ok(PageStart {
        offset: payload.o,
        page_size: payload.n.max(1),
        revision: payload.r,
    })
}

/// Refuse a cursor read at another store revision.
pub(crate) fn require_revision(page: &PageStart, current: Option<&str>) -> AppResult<()> {
    if current == Some(page.revision.as_str()) {
        return Ok(());
    }
    Err(AppError::conflict(
        "stale SPARQL cursor: the graph was written (or its store reopened) since the first page; \
         re-run the query without `cursor` to page a consistent answer",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minted_cursor_opens_and_an_edited_one_does_not() {
        let fingerprint = query_fingerprint("g", "SELECT * {}", &[]);
        let cursor = mint("g", &fingerprint, 50_000, 50_000, "1.2");
        assert_eq!(
            open(&cursor, "g", &fingerprint).unwrap(),
            PageStart {
                offset: 50_000,
                page_size: 50_000,
                revision: "1.2".into()
            }
        );
        let edited = cursor.replacen("sq1.", "sq1.e", 1);
        assert!(open(&edited, "g", &fingerprint).is_err());
        assert!(open(&cursor, "h", &fingerprint).is_err());
        assert_ne!(
            fingerprint,
            query_fingerprint("g", "SELECT * {}", &["urn:trash".into()])
        );
    }
}
