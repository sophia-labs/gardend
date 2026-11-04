//! garden-ledger: the ledger of streams.
//!
//! A ledger is a directory holding one append-only `{origin}.log` file per writer. Each file is a
//! dense sequence (seq 1, 2, 3, ...) of CRC-guarded frames; a frame is a canonical-JSON header
//! (whose blake3 is the event id) plus an opaque payload. Replicas converge by exchanging the
//! frames the other side lacks (version vector diff), dedup by position and refuse a fork. Torn
//! tails are cut on open (damage with valid frames after it is refused, never cut), and redaction is
//! a durable two-step: a `redact` frame, then the payload zeroed in place while the id stays
//! verifiable.
//!
//! # The shipping rule
//!
//! **A frame is shipped only once it cannot be lost.** The writer's ack rides `sync_barrier`
//! (sub-millisecond); a frame goes to a peer only after `sync_full` has covered it (the host issues
//! that on its window cadence, or eagerly when a peer waits). Open fsyncs the recovered file before
//! counting any of it as shippable. Every [`Ledger::diff`] chunk then repeats the peer's tip frame
//! and [`Ledger::apply`] compares its id: a mismatch is [`LedgerError::Fork`], a hard error that can
//! only mean a bug or tampering, never a crash in the window. Without both halves an origin that
//! lost a frame a peer already held could mint a different one at the same seq and no version
//! vector would ever say so.
//!
//! # What the host owns
//!
//! The crate owns no thread, timer or global: the host serializes access and calls the sync
//! primitives on its own cadence. The one lock is an advisory `flock` on each stream file, taken at
//! open and held for the stream's life, so a second writer on the same stream is refused with
//! [`StreamError::Locked`] instead of hoped away. See ARCHITECTURE.md section 3 (the ledger of
//! streams) and section 4 (sync).

pub mod frame;
pub mod hlc;
pub mod ledger;
pub mod stream;
pub mod sync;

pub use frame::{
    Event, FrameError, Header, Hlc, Id, Mode, OriginId, PayloadState, RedactPayload, Subject,
};
pub use hlc::HlcClock;
pub use ledger::{Draft, Ledger, LedgerError, VersionVector};
pub use stream::{Appended, FrameAt, Located, RawChunk, Recovery, Stream, StreamError};
