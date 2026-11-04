//! The seam between a room's loop and the state it hosts.
//!
//! A room speaks the y-sync wire (tags `0` sync with sub-tags 0/1/2, `1` awareness). What sits
//! behind SyncStep1 / SyncStep2 / Update is either a Y.Doc (state vector, update) or a ledger
//! (version vector, frame bundles). [`Convergent`] is that difference and nothing else: the
//! generic loop owns leases, the visitor gate, awareness and fan-out, and calls these methods
//! where it used to call yrs directly. See ARCHITECTURE.md sections 4 and 8, W6.md section 2.
//!
//! The async methods are written as `fn .. -> impl Future + Send` so a generic loop can hold the
//! future across an await inside a spawned task; implementors may still write plain `async fn`.

#![allow(dead_code)] // consumed by the generic room loop (lane R)

use std::fmt;
use std::future::Future;

use tokio::sync::broadcast;

use super::awareness::AwarenessTable;
use super::rooms::{ConnectionId, RoomFrame};
use crate::app_error::PersistenceError;

/// What a room hosts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum RoomKind {
    /// A Y.Doc (document, workspace, board).
    YDoc,
    /// A ledger of streams for one `(graph, vocab)`.
    Ledger,
}

impl RoomKind {
    /// The registry tag: `ydoc` or `ledger`.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            RoomKind::YDoc => "ydoc",
            RoomKind::Ledger => "ledger",
        }
    }
}

/// The outcome of applying a peer's update.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Applied {
    /// True when the state changed; the loop fans the update out only then.
    pub(crate) changed: bool,
    /// A payload for the sending peer only (yrs: none; ledger: none in W6, reserved for a fork report).
    pub(crate) reply: Option<Vec<u8>>,
}

/// Why a peer's payload, or a diff, could not be served.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ConvergeError {
    /// The payload did not decode (bad tag, short read, bad origin, trailing bytes).
    Decode(String),
    /// A peer's frame at `(origin, seq)` has a different id from ours. The ledger is untouched; the
    /// loop closes that connection with a logged reason.
    Fork {
        /// The origin that wrote two events at one position.
        origin: String,
        /// The position both frames claim.
        seq: u64,
    },
    /// The payload names a wire version this cell does not speak (`ledger-vv/N`, `ledger-bundle/N`).
    /// The loop closes the connection with a reason instead of skipping it, so the peer can tell.
    UnsupportedVersion {
        /// Which encoding (`ledger-vv`, `ledger-bundle`).
        what: &'static str,
        /// The version byte the peer sent.
        version: u8,
    },
    /// Decoded, but refused (gap, wrong origin, frame invalid, oversized, not allowed, room evicted).
    Refused(String),
    /// A stream file of the ledger is locked by another handle (an evicted room's last flush, or
    /// another process). The registry retries an open for a bounded time.
    Locked(String),
    /// Storage failed underneath.
    Persistence(PersistenceError),
}

impl fmt::Display for ConvergeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConvergeError::Decode(m) => write!(f, "undecodable payload: {m}"),
            ConvergeError::Fork { origin, seq } => {
                write!(
                    f,
                    "fork: origin {origin} wrote two different events at seq {seq}"
                )
            }
            ConvergeError::UnsupportedVersion { what, version } => {
                write!(f, "{what} version {version} unsupported")
            }
            ConvergeError::Refused(m) => write!(f, "refused: {m}"),
            ConvergeError::Locked(path) => write!(f, "ledger stream is locked: {path}"),
            ConvergeError::Persistence(e) => write!(f, "persistence: {e:?}"),
        }
    }
}

impl std::error::Error for ConvergeError {}

impl From<PersistenceError> for ConvergeError {
    fn from(e: PersistenceError) -> Self {
        ConvergeError::Persistence(e)
    }
}

/// State a room can host and converge with peers.
pub(crate) trait Convergent: Send + Sync + 'static {
    /// Which kind of state this is.
    fn kind(&self) -> RoomKind;

    /// What the peer should send us: a state vector (yrs) or a version vector (ledger), wire-encoded.
    fn version(&self) -> impl Future<Output = Vec<u8>> + Send;

    /// What a peer with `since` lacks, as wire payloads each under `max_bytes` (the websocket cap is
    /// 16 MiB; callers pass 8 MiB). A single frame larger than `max_bytes` still goes alone.
    fn diff(
        &self,
        since: &[u8],
        max_bytes: usize,
    ) -> impl Future<Output = Result<Vec<Vec<u8>>, ConvergeError>> + Send;

    /// The SyncStep2 payload that answers a SyncStep1 when `diff` has nothing to send. y-sync always
    /// answers (a Y.Doc diff is never empty, so `None` there); a ledger peer waits for its first SyncStep2
    /// to know it is in step, so the ledger returns an empty bundle.
    fn empty_diff(&self) -> Option<Vec<u8>> {
        None
    }

    /// A peer's update. `Applied::changed` drives fan-out; `Applied::reply` goes back to that peer only.
    fn apply(
        &self,
        payload: &[u8],
        from: ConnectionId,
    ) -> impl Future<Output = Result<Applied, ConvergeError>> + Send;

    /// Group commit within the window (yrs: persist_state; ledger: sync_barrier).
    fn persist(&self) -> impl Future<Output = Result<(), PersistenceError>> + Send;

    /// A durability point (yrs: no-op today; ledger: sync_full, after which frames may ship).
    fn durable(&self) -> impl Future<Output = Result<(), PersistenceError>> + Send;

    /// Payloads that became shippable since the last call (ledger only; yrs: empty). The loop fans
    /// these out as Update messages.
    fn shippable(&self) -> impl Future<Output = Vec<Vec<u8>>> + Send;

    /// Whether graph deletion or a registry eviction has invalidated this room. The generic loop
    /// checks it under the graph's lifecycle lease before every delivery; a room that was evicted
    /// refuses further updates.
    fn is_evicted(&self) -> bool;

    /// The ephemeral presence table.
    fn awareness(&self) -> &AwarenessTable;

    /// The fan-out channel every connection of this room subscribes to.
    fn frames(&self) -> &broadcast::Sender<RoomFrame>;
}
