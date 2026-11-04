//! garden-ledger: the ledger of streams.
//!
//! A ledger is a directory holding one append-only `{origin}.log` file per writer. Each file is a
//! dense sequence (seq 1, 2, 3, ...) of CRC-guarded frames; a frame is a canonical-JSON header
//! (whose blake3 is the event id) plus an opaque payload. Replicas converge by exchanging the
//! frames the other side lacks (version vector diff) and dedup by position. Torn tails are cut on
//! open, and redaction zeroes a payload in place while the id stays verifiable. The crate owns no
//! thread, timer, lock or global: the host serializes access and calls the sync primitives on its
//! own cadence. See ARCHITECTURE.md section 3 (the ledger of streams).

pub mod frame;
pub mod hlc;
pub mod ledger;
pub mod stream;
pub mod sync;

pub use frame::{Event, FrameError, Header, Hlc, Id, Mode, OriginId, Subject};
pub use hlc::HlcClock;
pub use ledger::{Draft, Ledger, LedgerError, VersionVector};
pub use stream::{Appended, FrameAt, RawChunk, Recovery, Stream, StreamError};
