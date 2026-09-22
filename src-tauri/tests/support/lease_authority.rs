//! A real, small HTTP lease authority for the garden integration boundary.
//!
//! This is deliberately a second implementation of the wire contract: axum
//! handles real TCP requests and rusqlite performs every conditional mutation
//! and event append in one SQL transaction. Test controls model authority
//! availability and handoff timing; they never call the cell implementation.

use axum::{
    extract::{Json, State},
    http::{header::AUTHORIZATION, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Router,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    net::TcpListener,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseRow {
    pub epoch: u64,
    pub holder: Option<String>,
    pub expires_at: i64,
    pub effective_at: i64,
    pub last_snap: u64,
    pub pending_snap: Option<u64>,
    pub state: String,
    pub stuck_since: Option<i64>,
    pub last_flush_ok_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseEvent {
    pub id: u64,
    pub kind: String,
    pub holder: String,
    pub epoch: u64,
    pub seq: Option<u64>,
    pub at_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimResult {
    pub epoch: u64,
    pub effective_at_ms: i64,
    pub last_snap: u64,
    pub pending_snap: Option<u64>,
}

#[derive(Clone)]
struct AppState {
    db: Arc<Mutex<Connection>>,
    token: String,
    ttl_ms: u64,
    stuck_flush_ms: u64,
    available: Arc<AtomicBool>,
    paused_renew_holders: Arc<Mutex<HashSet<String>>>,
    supersede_on_commit_holders: Arc<Mutex<HashSet<String>>>,
    fail_next_commit_holders: Arc<Mutex<HashSet<String>>>,
    publish_delay_ms: Arc<AtomicU64>,
    omit_epoch_on_publish_conflict: Arc<AtomicBool>,
    max_flush_in_progress_ms: Arc<AtomicU64>,
}

pub struct LeaseAuthority {
    db: Arc<Mutex<Connection>>,
    ttl_ms: u64,
    skew_ms: u64,
    port: u16,
    available: Arc<AtomicBool>,
    paused_renew_holders: Arc<Mutex<HashSet<String>>>,
    supersede_on_commit_holders: Arc<Mutex<HashSet<String>>>,
    fail_next_commit_holders: Arc<Mutex<HashSet<String>>>,
    publish_delay_ms: Arc<AtomicU64>,
    omit_epoch_on_publish_conflict: Arc<AtomicBool>,
    max_flush_in_progress_ms: Arc<AtomicU64>,
}

#[derive(Deserialize)]
struct RenewRequest {
    graph_id: String,
    holder: String,
    epoch: u64,
    flush_in_progress_ms: u64,
    last_flush_ok_ms: Option<u64>,
}

#[derive(Serialize)]
struct RenewResponse {
    epoch: u64,
    ttl_remaining_ms: u64,
    effective_in_ms: u64,
    state: String,
}

#[derive(Serialize)]
struct LeaseError {
    code: &'static str,
    current_epoch: Option<u64>,
}

#[derive(Deserialize)]
struct PublishRequest {
    graph_id: String,
    holder: String,
    epoch: u64,
    seq: u64,
    phase: String,
}

#[derive(Deserialize)]
struct ReleaseRequest {
    graph_id: String,
    holder: String,
    epoch: u64,
    reason: String,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_millis() as i64
}

fn i64_u64(value: u64) -> rusqlite::Result<i64> {
    i64::try_from(value).map_err(|_| {
        rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "u64 does not fit SQLite INTEGER",
        )))
    })
}

fn read_row(conn: &Connection, graph_id: &str) -> rusqlite::Result<Option<LeaseRow>> {
    conn.query_row(
        "SELECT epoch, holder, expires_at, effective_at, last_snap, pending_snap, \
         state, stuck_since, last_flush_ok_ms FROM leases WHERE graph_id = ?1",
        [graph_id],
        |row| {
            Ok(LeaseRow {
                epoch: row.get::<_, i64>(0)? as u64,
                holder: row.get(1)?,
                expires_at: row.get(2)?,
                effective_at: row.get(3)?,
                last_snap: row.get::<_, i64>(4)? as u64,
                pending_snap: row.get::<_, Option<i64>>(5)?.map(|value| value as u64),
                state: row.get(6)?,
                stuck_since: row.get(7)?,
                last_flush_ok_ms: row.get::<_, Option<i64>>(8)?.map(|value| value as u64),
            })
        },
    )
    .optional()
}

fn authorized(headers: &HeaderMap, token: &str) -> bool {
    let expected = format!("Bearer {token}");
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == expected)
}

fn unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"code": "authority_unavailable"})),
    )
        .into_response()
}

fn conflict(code: &'static str, current_epoch: Option<u64>) -> Response {
    (
        StatusCode::CONFLICT,
        Json(LeaseError {
            code,
            current_epoch,
        }),
    )
        .into_response()
}

fn append_event(
    conn: &Connection,
    kind: &str,
    graph_id: &str,
    holder: &str,
    epoch: u64,
    seq: Option<u64>,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO events (kind, graph_id, holder, epoch, seq, at_ms) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            kind,
            graph_id,
            holder,
            i64_u64(epoch)?,
            seq.map(i64_u64).transpose()?,
            now_ms()
        ],
    )?;
    Ok(())
}

async fn renew(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<RenewRequest>,
) -> Response {
    if !authorized(&headers, &state.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !state.available.load(Ordering::Acquire) {
        return unavailable();
    }
    if state
        .paused_renew_holders
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .contains(&body.holder)
    {
        return unavailable();
    }

    state
        .max_flush_in_progress_ms
        .fetch_max(body.flush_in_progress_ms, Ordering::AcqRel);
    let now = now_ms();
    let mut conn = state
        .db
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let tx = conn.transaction().expect("renew transaction");
    let row = read_row(&tx, &body.graph_id).expect("read lease");
    let valid = row.as_ref().is_some_and(|row| {
        row.holder.as_deref() == Some(&body.holder)
            && row.epoch == body.epoch
            && row.state == "active"
    });
    if !valid {
        let current_epoch = row.as_ref().map(|row| row.epoch);
        return conflict("lease_lost", current_epoch);
    }

    if state.stuck_flush_ms > 0 && body.flush_in_progress_ms > state.stuck_flush_ms {
        tx.execute(
            "UPDATE leases SET epoch = epoch + 1, holder = NULL, expires_at = ?1, \
             stuck_since = COALESCE(stuck_since, ?1) \
             WHERE graph_id = ?2 AND holder = ?3 AND epoch = ?4 AND state = 'active'",
            params![
                now,
                body.graph_id,
                body.holder,
                i64_u64(body.epoch).expect("epoch fits SQLite")
            ],
        )
        .expect("forfeit update");
        append_event(
            &tx,
            "forfeit",
            &body.graph_id,
            &body.holder,
            body.epoch,
            None,
        )
        .expect("forfeit event");
        tx.commit().expect("forfeit commit");
        return conflict("lease_forfeit", None);
    }

    let expires = now + i64_u64(state.ttl_ms).expect("ttl fits SQLite");
    tx.execute(
        "UPDATE leases SET expires_at = ?1, last_flush_ok_ms = ?2 \
         WHERE graph_id = ?3 AND holder = ?4 AND epoch = ?5 AND state = 'active'",
        params![
            expires,
            body.last_flush_ok_ms.map(i64_u64).transpose().unwrap(),
            body.graph_id,
            body.holder,
            i64_u64(body.epoch).expect("epoch fits SQLite")
        ],
    )
    .expect("renew update");
    tx.commit().expect("renew commit");
    let effective = row.expect("valid row").effective_at;
    (
        StatusCode::OK,
        Json(RenewResponse {
            epoch: body.epoch,
            ttl_remaining_ms: state.ttl_ms,
            // `i64::saturating_sub` saturates only at the integer bounds; it
            // can still return a small negative value when the effective
            // instant passed between claim and this first renew. Casting that
            // negative value to `u64` turns it into a centuries-long boot
            // delay, so clamp at zero exactly like the real authority.
            effective_in_ms: effective.saturating_sub(now).max(0) as u64,
            state: "active".into(),
        }),
    )
        .into_response()
}

async fn publish(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PublishRequest>,
) -> Response {
    if !authorized(&headers, &state.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !state.available.load(Ordering::Acquire) {
        return unavailable();
    }

    let delay_ms = state.publish_delay_ms.load(Ordering::Acquire);
    if delay_ms > 0 {
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
    }
    if !state.available.load(Ordering::Acquire) {
        return unavailable();
    }
    if body.phase == "commit"
        && state
            .fail_next_commit_holders
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&body.holder)
    {
        return unavailable();
    }

    let mut conn = state
        .db
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let tx = conn.transaction().expect("publish transaction");

    let supersede = body.phase == "commit"
        && state
            .supersede_on_commit_holders
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&body.holder);
    if supersede {
        let current = read_row(&tx, &body.graph_id)
            .expect("read lease for successor injection")
            .expect("lease exists for successor injection");
        assert_eq!(current.epoch, body.epoch);
        let successor_epoch = body.epoch + 1;
        let successor_holder = format!("successor-after-{}", body.holder);
        tx.execute(
            "UPDATE leases SET epoch = ?1, holder = ?2, expires_at = ?3, \
             effective_at = ?4, state = 'active' \
             WHERE graph_id = ?5 AND holder = ?6 AND epoch = ?7",
            params![
                i64_u64(successor_epoch).expect("successor epoch fits SQLite"),
                successor_holder,
                now_ms() + i64_u64(state.ttl_ms).expect("ttl fits SQLite"),
                now_ms(),
                body.graph_id,
                body.holder,
                i64_u64(body.epoch).expect("epoch fits SQLite")
            ],
        )
        .expect("inject successor at commit");
        append_event(
            &tx,
            "successor",
            &body.graph_id,
            &format!("successor-after-{}", body.holder),
            successor_epoch,
            None,
        )
        .expect("successor event");
    }

    let epoch = i64_u64(body.epoch).expect("epoch fits SQLite");
    let seq = i64_u64(body.seq).expect("seq fits SQLite");
    let changed = match body.phase.as_str() {
        "intent" => tx.execute(
            "UPDATE leases SET pending_snap = ?1 WHERE graph_id = ?2 AND holder = ?3 \
             AND epoch = ?4 AND state = 'active' \
             AND (pending_snap IS NULL OR pending_snap = ?1)",
            params![seq, body.graph_id, body.holder, epoch],
        ),
        "commit" => tx.execute(
            "UPDATE leases SET last_snap = ?1, pending_snap = NULL \
             WHERE graph_id = ?2 AND holder = ?3 AND epoch = ?4 \
             AND pending_snap = ?1",
            params![seq, body.graph_id, body.holder, epoch],
        ),
        "abort" => tx.execute(
            "UPDATE leases SET pending_snap = NULL \
             WHERE graph_id = ?1 AND holder = ?2 AND epoch = ?3 \
             AND pending_snap = ?4",
            params![body.graph_id, body.holder, epoch, seq],
        ),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"code": "bad_phase"})),
            )
                .into_response();
        }
    }
    .expect("publish update");

    if changed == 1 {
        append_event(
            &tx,
            &body.phase,
            &body.graph_id,
            &body.holder,
            body.epoch,
            Some(body.seq),
        )
        .expect("publish event");
        tx.commit().expect("publish commit");
        StatusCode::OK.into_response()
    } else {
        // Mirror platform-next's strongly-consistent post-CAS
        // classification. Loss of holder/epoch/active state is terminal
        // `lease_lost` (including same-epoch `retiring`); a phase/pending
        // mismatch under the otherwise-current lease is `lease_contended`
        // without an epoch. Garden treats that as restart-for-repair because
        // retrying a fresh intent must never replace the unresolved P.
        let row = read_row(&tx, &body.graph_id).expect("read current lease after CAS failure");
        let lease_lost = row.as_ref().is_none_or(|row| {
            row.holder.as_deref() != Some(body.holder.as_str())
                || row.epoch != body.epoch
                || row.state != "active"
        });
        tx.commit().expect("commit injected successor");
        if lease_lost {
            let current_epoch = row.as_ref().map_or(0, |row| row.epoch);
            conflict(
                "lease_lost",
                (!state.omit_epoch_on_publish_conflict.load(Ordering::Acquire))
                    .then_some(current_epoch),
            )
        } else {
            conflict("lease_contended", None)
        }
    }
}

async fn release(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ReleaseRequest>,
) -> Response {
    if !authorized(&headers, &state.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !state.available.load(Ordering::Acquire) {
        return unavailable();
    }
    let mut conn = state
        .db
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let tx = conn.transaction().expect("release transaction");
    let changed = tx
        .execute(
            "UPDATE leases SET holder = NULL, expires_at = ?1, state = 'active', \
             stuck_since = NULL WHERE graph_id = ?2 AND holder = ?3 AND epoch = ?4",
            params![
                now_ms(),
                body.graph_id,
                body.holder,
                i64_u64(body.epoch).expect("epoch fits SQLite")
            ],
        )
        .expect("release update");
    if changed == 1 {
        append_event(
            &tx,
            "release",
            &body.graph_id,
            &body.holder,
            body.epoch,
            None,
        )
        .expect("release event");
        tx.commit().expect("release commit");
        let _ = body.reason;
        StatusCode::NO_CONTENT.into_response()
    } else {
        let current_epoch = read_row(&tx, &body.graph_id)
            .expect("read current lease after release CAS failure")
            .map(|row| row.epoch);
        conflict("lease_lost", current_epoch)
    }
}

impl LeaseAuthority {
    pub fn start(token: &str, ttl_ms: u64, skew_ms: u64) -> Self {
        Self::start_with_stuck_flush(token, ttl_ms, skew_ms, 600_000)
    }

    pub fn start_with_stuck_flush(
        token: &str,
        ttl_ms: u64,
        skew_ms: u64,
        stuck_flush_ms: u64,
    ) -> Self {
        let conn = Connection::open_in_memory().expect("open authority sqlite");
        conn.execute_batch(
            "CREATE TABLE leases (
                graph_id TEXT PRIMARY KEY,
                epoch INTEGER NOT NULL,
                holder TEXT,
                expires_at INTEGER NOT NULL,
                effective_at INTEGER NOT NULL,
                last_snap INTEGER NOT NULL,
                pending_snap INTEGER,
                state TEXT NOT NULL DEFAULT 'active',
                stuck_since INTEGER,
                last_flush_ok_ms INTEGER
            );
            CREATE TABLE events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                kind TEXT NOT NULL,
                graph_id TEXT NOT NULL,
                holder TEXT NOT NULL,
                epoch INTEGER NOT NULL,
                seq INTEGER,
                at_ms INTEGER NOT NULL
            );",
        )
        .expect("create authority tables");
        let db = Arc::new(Mutex::new(conn));
        let available = Arc::new(AtomicBool::new(true));
        let paused_renew_holders = Arc::new(Mutex::new(HashSet::new()));
        let supersede_on_commit_holders = Arc::new(Mutex::new(HashSet::new()));
        let fail_next_commit_holders = Arc::new(Mutex::new(HashSet::new()));
        let publish_delay_ms = Arc::new(AtomicU64::new(0));
        let omit_epoch_on_publish_conflict = Arc::new(AtomicBool::new(false));
        let max_flush_in_progress_ms = Arc::new(AtomicU64::new(0));

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind lease authority");
        listener
            .set_nonblocking(true)
            .expect("set lease listener nonblocking");
        let port = listener
            .local_addr()
            .expect("lease listener address")
            .port();
        let state = AppState {
            db: db.clone(),
            token: token.to_string(),
            ttl_ms,
            stuck_flush_ms,
            available: available.clone(),
            paused_renew_holders: paused_renew_holders.clone(),
            supersede_on_commit_holders: supersede_on_commit_holders.clone(),
            fail_next_commit_holders: fail_next_commit_holders.clone(),
            publish_delay_ms: publish_delay_ms.clone(),
            omit_epoch_on_publish_conflict: omit_epoch_on_publish_conflict.clone(),
            max_flush_in_progress_ms: max_flush_in_progress_ms.clone(),
        };
        let router = Router::new()
            .route("/internal/lease/renew", post(renew))
            .route("/internal/lease/publish", post(publish))
            .route("/internal/lease/release", post(release))
            .with_state(state);
        thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("authority runtime");
            runtime.block_on(async {
                let listener =
                    tokio::net::TcpListener::from_std(listener).expect("tokio lease listener");
                axum::serve(listener, router)
                    .await
                    .expect("lease authority server");
            });
        });

        Self {
            db,
            ttl_ms,
            skew_ms,
            port,
            available,
            paused_renew_holders,
            supersede_on_commit_holders,
            fail_next_commit_holders,
            publish_delay_ms,
            omit_epoch_on_publish_conflict,
            max_flush_in_progress_ms,
        }
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}/internal/lease", self.port)
    }

    pub fn claim(&self, graph_id: &str, holder: &str, seed_last_snap: u64) -> ClaimResult {
        let now = now_ms();
        let skew = i64_u64(self.skew_ms).expect("skew fits SQLite");
        let mut conn = self
            .db
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let tx = conn.transaction().expect("claim transaction");
        let old = read_row(&tx, graph_id).expect("read old lease");
        let epoch = old.as_ref().map_or(1, |row| row.epoch + 1);
        let effective = old
            .as_ref()
            .map_or(now, |row| now.max(row.expires_at + skew));
        let last_snap = old.as_ref().map_or(seed_last_snap, |row| row.last_snap);
        let pending_snap = old.as_ref().and_then(|row| row.pending_snap);
        let expires = now + i64_u64(self.ttl_ms).expect("ttl fits SQLite");
        let changed = tx
            .execute(
                "INSERT INTO leases (
                    graph_id, epoch, holder, expires_at, effective_at, last_snap,
                    pending_snap, state, stuck_since, last_flush_ok_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, 'active', NULL, NULL)
                 ON CONFLICT(graph_id) DO UPDATE SET
                    epoch = excluded.epoch,
                    holder = excluded.holder,
                    expires_at = excluded.expires_at,
                    effective_at = excluded.effective_at,
                    last_snap = excluded.last_snap,
                    state = 'active'
                 WHERE leases.holder IS NULL
                    OR leases.expires_at < ?7
                    OR leases.state != 'active'
                    OR leases.holder = ?3",
                params![
                    graph_id,
                    i64_u64(epoch).expect("epoch fits SQLite"),
                    holder,
                    expires,
                    effective,
                    i64_u64(last_snap).expect("snapshot fits SQLite"),
                    now
                ],
            )
            .expect("claim update");
        assert_eq!(
            changed, 1,
            "lease claim is contended by a live different holder"
        );
        append_event(&tx, "claim", graph_id, holder, epoch, None).expect("claim event");
        tx.commit().expect("claim commit");
        ClaimResult {
            epoch,
            effective_at_ms: effective,
            last_snap,
            pending_snap,
        }
    }

    pub fn seed_expired(&self, graph_id: &str, epoch: u64, last_snap: u64) {
        self.seed_with_expiry(graph_id, epoch, last_snap, now_ms() - 1);
    }

    fn seed_with_expiry(&self, graph_id: &str, epoch: u64, last_snap: u64, expires_at: i64) {
        let conn = self
            .db
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        conn.execute(
            "INSERT OR REPLACE INTO leases (
                graph_id, epoch, holder, expires_at, effective_at, last_snap,
                pending_snap, state, stuck_since, last_flush_ok_ms
             ) VALUES (?1, ?2, NULL, ?3, ?3, ?4, NULL, 'active', NULL, NULL)",
            params![
                graph_id,
                i64_u64(epoch).expect("epoch fits SQLite"),
                expires_at,
                i64_u64(last_snap).expect("snapshot fits SQLite")
            ],
        )
        .expect("seed lease");
    }

    pub fn describe(&self, graph_id: &str) -> Option<LeaseRow> {
        let conn = self
            .db
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        read_row(&conn, graph_id).expect("describe lease")
    }

    pub fn retire(&self, graph_id: &str) {
        let conn = self
            .db
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(
            conn.execute(
                "UPDATE leases SET state = 'retiring' WHERE graph_id = ?1",
                [graph_id],
            )
            .expect("retire lease"),
            1,
            "retire requires an existing lease"
        );
    }

    pub fn events(&self, graph_id: &str) -> Vec<LeaseEvent> {
        let conn = self
            .db
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut statement = conn
            .prepare(
                "SELECT id, kind, holder, epoch, seq, at_ms FROM events \
                 WHERE graph_id = ?1 ORDER BY id",
            )
            .expect("prepare events query");
        statement
            .query_map([graph_id], |row| {
                Ok(LeaseEvent {
                    id: row.get::<_, i64>(0)? as u64,
                    kind: row.get(1)?,
                    holder: row.get(2)?,
                    epoch: row.get::<_, i64>(3)? as u64,
                    seq: row.get::<_, Option<i64>>(4)?.map(|value| value as u64),
                    at_ms: row.get(5)?,
                })
            })
            .expect("query events")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("read events")
    }

    pub fn set_available(&self, available: bool) {
        self.available.store(available, Ordering::Release);
    }

    pub fn pause_renew(&self, holder: &str, paused: bool) {
        let mut holders = self
            .paused_renew_holders
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if paused {
            holders.insert(holder.to_string());
        } else {
            holders.remove(holder);
        }
    }

    pub fn supersede_on_next_commit(&self, holder: &str) {
        self.supersede_on_commit_holders
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(holder.to_string());
    }

    pub fn fail_next_commit(&self, holder: &str) {
        self.fail_next_commit_holders
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(holder.to_string());
    }

    pub fn set_publish_delay_ms(&self, delay_ms: u64) {
        self.publish_delay_ms.store(delay_ms, Ordering::Release);
    }

    pub fn omit_epoch_on_publish_conflict(&self, omit: bool) {
        self.omit_epoch_on_publish_conflict
            .store(omit, Ordering::Release);
    }

    pub fn max_flush_in_progress_ms(&self) -> u64 {
        self.max_flush_in_progress_ms.load(Ordering::Acquire)
    }

    pub fn wait_until_expired(&self, graph_id: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self
                .describe(graph_id)
                .is_some_and(|row| row.expires_at < now_ms())
            {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::Client;

    #[test]
    fn real_http_claim_renew_publish_and_release_round_trip() {
        let authority = LeaseAuthority::start("test-token", 20_000, 5);
        let claim = authority.claim("g", "h", 0);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let client = Client::new();
            let response = client
                .post(format!("{}/renew", authority.base_url()))
                .bearer_auth("test-token")
                .json(&serde_json::json!({
                    "graph_id": "g",
                    "holder": "h",
                    "epoch": claim.epoch,
                    "flush_in_progress_ms": 0
                }))
                .send()
                .await
                .unwrap();
            assert!(response.status().is_success());
            for phase in ["intent", "commit"] {
                let response = client
                    .post(format!("{}/publish", authority.base_url()))
                    .bearer_auth("test-token")
                    .json(&serde_json::json!({
                        "graph_id": "g",
                        "holder": "h",
                        "epoch": claim.epoch,
                        "seq": 1,
                        "phase": phase
                    }))
                    .send()
                    .await
                    .unwrap();
                assert!(response.status().is_success());
            }
            let response = client
                .post(format!("{}/release", authority.base_url()))
                .bearer_auth("test-token")
                .json(&serde_json::json!({
                    "graph_id": "g",
                    "holder": "h",
                    "epoch": claim.epoch,
                    "reason": "test"
                }))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
        });
        let row = authority.describe("g").unwrap();
        assert_eq!(row.last_snap, 1);
        assert_eq!(row.holder, None);
        assert_eq!(
            authority
                .events("g")
                .iter()
                .map(|event| event.kind.as_str())
                .collect::<Vec<_>>(),
            ["claim", "intent", "commit", "release"]
        );
    }

    #[test]
    fn real_http_renew_reports_lease_lost_and_forfeit() {
        let authority = LeaseAuthority::start_with_stuck_flush("test-token", 20_000, 5, 10);
        let claim = authority.claim("g", "h", 0);
        let before_expiry = authority.describe("g").unwrap().expires_at;
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let client = Client::new();
            let response = client
                .post(format!("{}/renew", authority.base_url()))
                .bearer_auth("test-token")
                .json(&serde_json::json!({
                    "graph_id": "g",
                    "holder": "z",
                    "epoch": claim.epoch,
                    "flush_in_progress_ms": 0
                }))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::CONFLICT);
            assert_eq!(
                response.json::<serde_json::Value>().await.unwrap()["code"],
                "lease_lost"
            );

            let response = client
                .post(format!("{}/renew", authority.base_url()))
                .bearer_auth("test-token")
                .json(&serde_json::json!({
                    "graph_id": "g",
                    "holder": "h",
                    "epoch": claim.epoch,
                    "flush_in_progress_ms": 11
                }))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::CONFLICT);
            assert_eq!(
                response.json::<serde_json::Value>().await.unwrap()["code"],
                "lease_forfeit"
            );
        });
        let row = authority.describe("g").unwrap();
        assert_eq!(row.state, "active");
        assert_eq!(row.epoch, claim.epoch + 1);
        assert_eq!(row.holder, None);
        assert!(row.stuck_since.is_some());
        assert_eq!(authority.max_flush_in_progress_ms(), 11);
        assert!(
            row.expires_at <= before_expiry,
            "forfeit must expire now, never extend the old deadline"
        );
    }

    #[test]
    fn real_http_intent_never_overwrites_a_different_pending_snapshot() {
        let authority = LeaseAuthority::start("test-token", 20_000, 5);
        let claim = authority.claim("g", "h", 0);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let client = Client::new();
            let publish = |seq| {
                client
                    .post(format!("{}/publish", authority.base_url()))
                    .bearer_auth("test-token")
                    .json(&serde_json::json!({
                        "graph_id": "g",
                        "holder": "h",
                        "epoch": claim.epoch,
                        "seq": seq,
                        "phase": "intent"
                    }))
                    .send()
            };
            assert!(publish(1).await.unwrap().status().is_success());
            let response = publish(2).await.unwrap();
            assert_eq!(response.status(), StatusCode::CONFLICT);
            assert_eq!(
                response.json::<serde_json::Value>().await.unwrap()["code"],
                "lease_contended"
            );
        });
        let row = authority.describe("g").unwrap();
        assert_eq!(row.pending_snap, Some(1));
        let successor = authority.claim("g", "h", 0);
        assert_eq!(successor.pending_snap, Some(1));
    }

    #[test]
    fn real_http_publish_classifies_same_epoch_retiring_as_lease_lost() {
        let authority = LeaseAuthority::start("test-token", 20_000, 5);
        let claim = authority.claim("g", "h", 0);
        authority.retire("g");
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let response = Client::new()
                .post(format!("{}/publish", authority.base_url()))
                .bearer_auth("test-token")
                .json(&serde_json::json!({
                    "graph_id": "g",
                    "holder": "h",
                    "epoch": claim.epoch,
                    "seq": 1,
                    "phase": "intent"
                }))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::CONFLICT);
            let body = response.json::<serde_json::Value>().await.unwrap();
            assert_eq!(body["code"], "lease_lost");
            assert_eq!(body["current_epoch"], claim.epoch);
        });
    }
}
