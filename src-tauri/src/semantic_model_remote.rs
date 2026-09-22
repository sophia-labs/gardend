//! Remote embeddings pool identity: everything needed to answer "is a remote
//! pool configured, and what does it look like" for the headless-cell path
//! (`GARDEN_EMBEDDINGS_URL` set, e.g. a platform-next cell talking to a
//! text-embeddings-inference-compatible pool). Deliberately has no
//! crate-internal dependencies beyond `ureq`/`serde_json`/`std` — this is a
//! leaf module every semantic consumer's choke point (`semantic_model_state`,
//! `semantic_model_catalog`) reaches into, not the other way around.
use std::sync::{Mutex, OnceLock};

/// Env var that switches embedding compute to a remote pool (platform-next
/// cells). When set, ALL models embed via HTTP against a
/// text-embeddings-inference-compatible service; local backends (ONNX dylib,
/// candle weights) are never touched. Desktop builds without the env are
/// unaffected.
pub(crate) const REMOTE_EMBEDDINGS_ENV: &str = "GARDEN_EMBEDDINGS_URL";

pub(crate) fn remote_embeddings_endpoint() -> Option<String> {
    std::env::var(REMOTE_EMBEDDINGS_ENV)
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
}

/// Fixed routing key: what `read_semantic_model_config` writes as
/// `selected_model_id` on the remote path, and what `semantic_model_spec_by_id`
/// pattern-matches to enter the remote branch. NEVER derived from the pool —
/// must stay constant across probes so repeated lookups keep resolving.
pub(crate) const REMOTE_POOL_MODEL_ID: &str = "remote/embeddings-pool";

#[derive(Clone, Copy)]
pub(crate) struct RemotePoolIdentity {
    pub(crate) dimensions: usize,
    /// Real TEI-reported model id, leaked once to 'static (see
    /// `probe_remote_pool_identity`) — or `REMOTE_POOL_MODEL_ID` if `/info`
    /// is unavailable/unparseable.
    pub(crate) pool_model_id: &'static str,
}

// Keyed by endpoint (not just Option<T>) so a changed GARDEN_EMBEDDINGS_URL —
// and, just as importantly, two tests pointing at two different fake pools in
// the same test binary — both re-probe instead of reusing a stale
// cross-endpoint cache entry. This deliberately mirrors the
// `SEMANTIC_EMBEDDER: OnceLock<Mutex<Option<LoadedSemanticEmbedder>>>` pattern
// in `semantic_embedder.rs`, not a brand-new idiom.
static REMOTE_POOL_IDENTITY: OnceLock<Mutex<Option<(String, RemotePoolIdentity)>>> =
    OnceLock::new();

/// Resolve (and cache, per-endpoint, once per process) the embeddings pool's
/// vector dimensionality and reported model id. Failures are never cached —
/// the slot stays untouched on error — so a transient pool blip retries on
/// the very next call, exactly how `ensure_semantic_embedder` already behaves
/// when `load_semantic_embedder_backend` fails today.
pub(crate) fn remote_pool_identity(endpoint: &str) -> Result<RemotePoolIdentity, String> {
    let cell = REMOTE_POOL_IDENTITY.get_or_init(|| Mutex::new(None));
    let mut guard = cell
        .lock()
        .map_err(|_| "remote pool identity lock was poisoned".to_string())?;
    if let Some((cached_endpoint, identity)) = guard.as_ref() {
        if cached_endpoint == endpoint {
            return Ok(*identity);
        }
    }
    let identity = probe_remote_pool_identity(endpoint)?;
    *guard = Some((endpoint.to_string(), identity));
    Ok(identity)
}

/// Reuses the *exact* request shape `RemoteEmbedder::embed` already sends
/// (`POST {endpoint}/embed`, `{"inputs": [..], "truncate": true}`), so the
/// probe exercises the identical code path production traffic will hit.
fn probe_dimensions(agent: &ureq::Agent, endpoint: &str) -> Result<usize, String> {
    const REMOTE_PROBE_INPUT: &str = "garden-embeddings-dimension-probe";
    let url = format!("{endpoint}/embed");
    let mut response = agent
        .post(&url)
        .send_json(serde_json::json!({ "inputs": [REMOTE_PROBE_INPUT], "truncate": true }))
        .map_err(|error| format!("embeddings pool dimension probe failed ({url}): {error}"))?;
    let vectors: Vec<Vec<f32>> = response.body_mut().read_json().map_err(|error| {
        format!("embeddings pool dimension probe returned invalid JSON: {error}")
    })?;
    let vector = vectors
        .into_iter()
        .next()
        .ok_or_else(|| format!("embeddings pool dimension probe returned no vectors ({url})"))?;
    if vector.is_empty() {
        return Err(format!(
            "embeddings pool dimension probe returned an empty vector ({url})"
        ));
    }
    Ok(vector.len())
}

/// `GET {endpoint}/info` (best-effort, `model_id` field) supplies the dynamic
/// pool-reported model id.
fn fetch_pool_model_id(agent: &ureq::Agent, endpoint: &str) -> Option<String> {
    let mut response = agent.get(&format!("{endpoint}/info")).call().ok()?;
    let body: serde_json::Value = response.body_mut().read_json().ok()?;
    body.get("model_id")?.as_str().map(str::to_string)
}

/// Leak-once, not leak-per-call: the fetched model id string is leaked to
/// 'static exactly once here, inside the same probe call gated by the
/// `REMOTE_POOL_IDENTITY` cache above, so `SemanticModelSpec.model_id:
/// &'static str` can legitimately hold the *real* pool model id without an
/// unbounded leak over a long-lived cell process.
///
/// Accepted residual limitation (reindex/search compat-check mitigation, not
/// full hot-swap detection — deliberately no background re-probe loop; we
/// control the pool, it's a fixed model pinned via helm):
///   - The real `pool_model_id` this returns DOES get recorded as the
///     effective model identity everywhere downstream (`SemanticIndexManifest.
///     model_id`, the search-time compat check) instead of the opaque
///     `REMOTE_POOL_MODEL_ID` sentinel, so a pool model swap is detected as an
///     index/model mismatch — but only across a process recycle: this probe
///     is gated by `REMOTE_POOL_IDENTITY`, which caches per-endpoint for the
///     lifetime of the process, so a swap behind the *same* URL while this
///     process keeps running is invisible until the cell restarts and probes
///     cold again.
///   - If `/info` is unavailable/unparseable, `pool_model_id` falls back to
///     the constant `REMOTE_POOL_MODEL_ID` sentinel (see below). A swap to a
///     *different* model at the *same* dimensionality is then undetectable
///     even across a recycle: neither the sentinel model id nor the
///     dimensions change, and dimensions are the only other signal the
///     compat check has.
fn probe_remote_pool_identity(endpoint: &str) -> Result<RemotePoolIdentity, String> {
    let agent = ureq::Agent::new_with_defaults();
    let dimensions = probe_dimensions(&agent, endpoint)?;
    let pool_model_id: &'static str = match fetch_pool_model_id(&agent, endpoint) {
        Some(id) => Box::leak(id.into_boxed_str()),
        None => {
            log::warn!(
                "embeddings pool {endpoint}/info unavailable; falling back to sentinel model id for reindex-change detection"
            );
            REMOTE_POOL_MODEL_ID
        }
    };
    Ok(RemotePoolIdentity {
        dimensions,
        pool_model_id,
    })
}

/// Test-only support shared across every file that needs a real (no mock
/// library) fake embeddings pool: a hand-rolled HTTP/1.1 server over a real
/// `std::net::TcpListener`, driving the real `ureq` client through the exact
/// request/response bytes production traffic would see.
#[cfg(test)]
pub(crate) mod test_support {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    /// The ONE process-wide mutex for tests that mutate `GARDEN_EMBEDDINGS_URL`
    /// (a process-global env var) or rely on exact hit/miss behavior of the
    /// single-slot `REMOTE_POOL_IDENTITY` cache above — both are shared
    /// process state, so every such test (in this file or any other) must
    /// hold this lock for its full body. Mirrors
    /// `tauri_runtime::profile_env_serial()`'s role for `GARDEN_PROFILE_DIR`.
    pub(crate) fn remote_embeddings_test_serial() -> &'static std::sync::Mutex<()> {
        static SERIAL: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        SERIAL.get_or_init(|| std::sync::Mutex::new(()))
    }

    /// Spawn a fake embeddings pool on a real loopback TCP port. `handler`
    /// receives `(method, path, body_bytes)` for each non-health request and
    /// returns `(status, response_body)`. Every fake pool implements the
    /// production backend's mandatory `GET /health` probe automatically, so
    /// individual semantic fixtures cannot accidentally test a weaker server
    /// contract. The listener thread runs for the lifetime of the test binary
    /// (not joined) — acceptable for test-only use.
    pub(crate) fn spawn_fake_pool<F>(mut handler: F) -> String
    where
        F: FnMut(&str, &str, &[u8]) -> (u16, String) + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake embeddings pool");
        let port = listener.local_addr().expect("fake pool local addr").port();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let Some((method, path, body)) = read_request(&mut stream) else {
                    continue;
                };
                let (status, response_body) = if method == "GET" && path == "/health" {
                    (200, "{}".to_string())
                } else {
                    handler(&method, &path, &body)
                };
                write_response(&mut stream, status, &response_body);
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    fn read_request(stream: &mut TcpStream) -> Option<(String, String, Vec<u8>)> {
        let mut reader = BufReader::new(stream.try_clone().ok()?);
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).ok()? == 0 {
            return None;
        }
        let mut parts = request_line.split_whitespace();
        let method = parts.next()?.to_string();
        let path = parts.next()?.to_string();
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            let read = reader.read_line(&mut line).ok()?;
            if read == 0 || line == "\r\n" || line == "\n" {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; content_length];
        if content_length > 0 {
            reader.read_exact(&mut body).ok()?;
        }
        Some((method, path, body))
    }

    fn write_response(stream: &mut TcpStream, status: u16, body: &str) {
        let status_text = match status {
            200 => "OK",
            404 => "Not Found",
            500 => "Internal Server Error",
            _ => "OK",
        };
        let response = format!(
            "HTTP/1.1 {status} {status_text}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes());
        let _ = stream.flush();
    }

    /// A JSON `[[v, v, ...], ...]` body with `count` vectors of `dimensions`
    /// values each — the shape `POST /embed` returns in production.
    pub(crate) fn embed_vectors_response(count: usize, dimensions: usize) -> String {
        let vector: Vec<f64> = vec![0.125; dimensions];
        let vectors: Vec<Vec<f64>> = std::iter::repeat(vector).take(count.max(1)).collect();
        serde_json::to_string(&vectors).expect("serialize fake embeddings response")
    }

    /// How many strings the real `RemoteEmbedder::embed`/dimension-probe sent
    /// in this request's `{"inputs": [...]}` body — so a fake pool can return
    /// a vector count that matches what the real pool-mismatch validation in
    /// `RemoteEmbedder::embed` requires (`vectors.len() == batch.len()`).
    pub(crate) fn embed_input_count(body: &[u8]) -> usize {
        serde_json::from_slice::<serde_json::Value>(body)
            .ok()
            .and_then(|value| {
                value
                    .get("inputs")
                    .and_then(|inputs| inputs.as_array().map(|array| array.len()))
            })
            .unwrap_or(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_support::{embed_input_count, embed_vectors_response, remote_embeddings_test_serial};

    #[test]
    fn remote_pool_identity_probes_dimensions_via_embed() {
        let _serial = remote_embeddings_test_serial()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let endpoint = test_support::spawn_fake_pool(|_method, path, body| match path {
            "/embed" => (200, embed_vectors_response(embed_input_count(body), 384)),
            _ => (404, "not found".to_string()),
        });

        let identity = remote_pool_identity(&endpoint).expect("probe succeeds");
        assert_eq!(identity.dimensions, 384);
    }

    #[test]
    fn remote_pool_identity_reads_model_id_from_info() {
        let _serial = remote_embeddings_test_serial()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let endpoint = test_support::spawn_fake_pool(|_method, path, body| match path {
            "/embed" => (200, embed_vectors_response(embed_input_count(body), 768)),
            "/info" => (
                200,
                serde_json::json!({ "model_id": "bge-base-en-v1.5" }).to_string(),
            ),
            _ => (404, "not found".to_string()),
        });

        let identity = remote_pool_identity(&endpoint).expect("probe succeeds");
        assert_eq!(identity.dimensions, 768);
        assert_eq!(identity.pool_model_id, "bge-base-en-v1.5");
    }

    #[test]
    fn remote_pool_identity_falls_back_to_sentinel_when_info_missing() {
        let _serial = remote_embeddings_test_serial()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let endpoint = test_support::spawn_fake_pool(|_method, path, body| match path {
            "/embed" => (200, embed_vectors_response(embed_input_count(body), 384)),
            "/info" => (404, "not found".to_string()),
            _ => (404, "not found".to_string()),
        });

        let identity = remote_pool_identity(&endpoint).expect("probe succeeds");
        assert_eq!(identity.pool_model_id, REMOTE_POOL_MODEL_ID);
    }

    #[test]
    fn remote_pool_identity_caches_per_endpoint_not_globally() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let _serial = remote_embeddings_test_serial()
            .lock()
            .unwrap_or_else(|p| p.into_inner());

        let calls_a = Arc::new(AtomicUsize::new(0));
        let calls_a_clone = calls_a.clone();
        let endpoint_a = test_support::spawn_fake_pool(move |_method, path, body| {
            if path == "/embed" {
                calls_a_clone.fetch_add(1, Ordering::SeqCst);
                (200, embed_vectors_response(embed_input_count(body), 384))
            } else {
                (404, "not found".to_string())
            }
        });
        let calls_b = Arc::new(AtomicUsize::new(0));
        let calls_b_clone = calls_b.clone();
        let endpoint_b = test_support::spawn_fake_pool(move |_method, path, body| {
            if path == "/embed" {
                calls_b_clone.fetch_add(1, Ordering::SeqCst);
                (200, embed_vectors_response(embed_input_count(body), 768))
            } else {
                (404, "not found".to_string())
            }
        });

        let identity_a = remote_pool_identity(&endpoint_a).expect("probe a");
        assert_eq!(identity_a.dimensions, 384);
        assert_eq!(calls_a.load(Ordering::SeqCst), 1);

        let identity_a_again = remote_pool_identity(&endpoint_a).expect("cache hit a");
        assert_eq!(identity_a_again.dimensions, 384);
        assert_eq!(
            calls_a.load(Ordering::SeqCst),
            1,
            "second call for the same endpoint must hit the cache"
        );

        let identity_b = remote_pool_identity(&endpoint_b).expect("probe b — different endpoint");
        assert_eq!(identity_b.dimensions, 768);
        assert_eq!(
            calls_b.load(Ordering::SeqCst),
            1,
            "a different endpoint must not reuse endpoint a's cached identity"
        );
    }

    #[test]
    fn remote_pool_identity_does_not_cache_probe_failure() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let _serial = remote_embeddings_test_serial()
            .lock()
            .unwrap_or_else(|p| p.into_inner());

        let failing = Arc::new(AtomicBool::new(true));
        let failing_clone = failing.clone();
        let endpoint = test_support::spawn_fake_pool(move |_method, path, body| {
            if path == "/embed" {
                if failing_clone.load(Ordering::SeqCst) {
                    (500, "boom".to_string())
                } else {
                    (200, embed_vectors_response(embed_input_count(body), 384))
                }
            } else {
                (404, "not found".to_string())
            }
        });

        let first = remote_pool_identity(&endpoint);
        assert!(
            first.is_err(),
            "pool-down probe must fail, not cache a bogus identity"
        );

        failing.store(false, Ordering::SeqCst);
        let second =
            remote_pool_identity(&endpoint).expect("retry after the pool recovers succeeds");
        assert_eq!(second.dimensions, 384);
    }

    #[test]
    fn remote_pool_identity_errors_when_embed_returns_empty_vector() {
        let _serial = remote_embeddings_test_serial()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let endpoint = test_support::spawn_fake_pool(|_method, path, _body| match path {
            "/embed" => (200, "[[]]".to_string()),
            _ => (404, "not found".to_string()),
        });

        let error = match remote_pool_identity(&endpoint) {
            Ok(_) => panic!("empty embedding vector unexpectedly resolved as a pool identity"),
            Err(error) => error,
        };
        assert!(error.contains("empty vector"), "got: {error}");
    }
}
