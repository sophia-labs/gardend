//! Bounded, treatment-neutral diagnostics for the durable cell flush.
//!
//! This is intentionally separate from the frozen Observatory CaptureEvent
//! contract. When `GARDEN_FLUSH_CAUSALITY_TRACE=1`, one bounded JSON summary is
//! written through `log` (stderr in `gardend`) for each attempted flush. No
//! snapshot decision, dirty decision, cadence, or durable format depends on
//! this module.

use serde::Serialize;
use std::{
    cell::{Cell, RefCell},
    path::Path,
    sync::{atomic::AtomicU64, atomic::Ordering, OnceLock},
    time::{Duration, Instant},
};

pub(crate) const TRACE_ENV: &str = "GARDEN_FLUSH_CAUSALITY_TRACE";
const TRACE_SCHEMA: &str = "garden.flush_dirty_causality.v1";
const TRACE_EVENT: &str = "durable_flush.span_completed";
const MAX_STORE_DETAILS: usize = 4;
const MAX_SPARSE_COUNTS: usize = 8;
const MAX_EVENT_BYTES: usize = 3_800;

pub(crate) fn tracing_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var(TRACE_ENV)
            .ok()
            .is_some_and(|value| matches!(value.trim(), "1" | "true" | "TRUE" | "yes" | "YES"))
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FlushTrigger {
    PeriodicDebounce,
    RpoCeiling,
    PostImport,
    FinalShutdown,
    ObservatoryApply,
    ManualPeriodic,
    ManualForced,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DirtyReason {
    WriteGuardCompleted,
    RdfDirectHook,
    GraphLeaseFallback,
    EmporiumGateFallback,
    StoreFirstObserved,
    StoreIdentityReplaced,
    PreviousSnapshotMissing,
    BookkeepingUnknown,
}

impl DirtyReason {
    const COUNT: usize = Self::BookkeepingUnknown as usize + 1;

    fn index(self) -> usize {
        self as usize
    }

    fn label(self) -> &'static str {
        match self {
            Self::WriteGuardCompleted => "write_guard_completed",
            Self::RdfDirectHook => "rdf_direct_hook",
            Self::GraphLeaseFallback => "graph_lease_fallback",
            Self::EmporiumGateFallback => "emporium_gate_fallback",
            Self::StoreFirstObserved => "store_first_observed",
            Self::StoreIdentityReplaced => "store_identity_replaced",
            Self::PreviousSnapshotMissing => "previous_snapshot_missing",
            Self::BookkeepingUnknown => "bookkeeping_unknown",
        }
    }

    const ALL: [Self; Self::COUNT] = [
        Self::WriteGuardCompleted,
        Self::RdfDirectHook,
        Self::GraphLeaseFallback,
        Self::EmporiumGateFallback,
        Self::StoreFirstObserved,
        Self::StoreIdentityReplaced,
        Self::PreviousSnapshotMissing,
        Self::BookkeepingUnknown,
    ];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DirtyOrigin {
    CrdtEnqueue,
    CrdtApply,
    WebsocketRoomOpen,
    WebsocketUpdate,
    GraphLifecycle,
    DocumentPersistence,
    DocumentHistory,
    OriginalFile,
    LocalJobDb,
    ProfileMetadataDb,
    OperationJournal,
    OperationCompletionLedger,
    SemanticIndex,
    RdfApi,
    RdfSeed,
    RdfMaterializer,
    ObservatoryApply,
    Emporium,
    DurabilityInternal,
    Other,
}

impl DirtyOrigin {
    const COUNT: usize = Self::Other as usize + 1;

    fn index(self) -> usize {
        self as usize
    }

    fn label(self) -> &'static str {
        match self {
            Self::CrdtEnqueue => "crdt_enqueue",
            Self::CrdtApply => "crdt_apply",
            Self::WebsocketRoomOpen => "websocket_room_open",
            Self::WebsocketUpdate => "websocket_update",
            Self::GraphLifecycle => "graph_lifecycle",
            Self::DocumentPersistence => "document_persistence",
            Self::DocumentHistory => "document_history",
            Self::OriginalFile => "original_file",
            Self::LocalJobDb => "local_job_db",
            Self::ProfileMetadataDb => "profile_metadata_db",
            Self::OperationJournal => "operation_journal",
            Self::OperationCompletionLedger => "operation_completion_ledger",
            Self::SemanticIndex => "semantic_index",
            Self::RdfApi => "rdf_api",
            Self::RdfSeed => "rdf_seed",
            Self::RdfMaterializer => "rdf_materializer",
            Self::ObservatoryApply => "observatory_apply",
            Self::Emporium => "emporium",
            Self::DurabilityInternal => "durability_internal",
            Self::Other => "other",
        }
    }

    const ALL: [Self; Self::COUNT] = [
        Self::CrdtEnqueue,
        Self::CrdtApply,
        Self::WebsocketRoomOpen,
        Self::WebsocketUpdate,
        Self::GraphLifecycle,
        Self::DocumentPersistence,
        Self::DocumentHistory,
        Self::OriginalFile,
        Self::LocalJobDb,
        Self::ProfileMetadataDb,
        Self::OperationJournal,
        Self::OperationCompletionLedger,
        Self::SemanticIndex,
        Self::RdfApi,
        Self::RdfSeed,
        Self::RdfMaterializer,
        Self::ObservatoryApply,
        Self::Emporium,
        Self::DurabilityInternal,
        Self::Other,
    ];
}

/// Convert a compile-time source location into a fixed-cardinality subsystem.
/// The raw path and line are deliberately never serialized.
pub(crate) fn origin_from_location(
    location: &'static std::panic::Location<'static>,
) -> DirtyOrigin {
    origin_from_file(location.file())
}

fn origin_from_file(file: &str) -> DirtyOrigin {
    if file.ends_with("/crdt_engine/executor.rs") {
        DirtyOrigin::CrdtApply
    } else if file.ends_with("/crdt_queue.rs") {
        DirtyOrigin::CrdtEnqueue
    } else if file.ends_with("/loopback_hocuspocus_routes.rs") {
        DirtyOrigin::WebsocketRoomOpen
    } else if file.ends_with("/crdt_engine/rooms.rs") {
        DirtyOrigin::WebsocketUpdate
    } else if file.ends_with("/local_job_db.rs") {
        DirtyOrigin::LocalJobDb
    } else if file.ends_with("/profile_metadata_db.rs") {
        DirtyOrigin::ProfileMetadataDb
    } else if file.ends_with("/crdt_operation_journal.rs") {
        DirtyOrigin::OperationJournal
    } else if file.ends_with("/operation_completion_ledger.rs") {
        DirtyOrigin::OperationCompletionLedger
    } else if file.contains("document_history") {
        DirtyOrigin::DocumentHistory
    } else if file.contains("original_file") {
        DirtyOrigin::OriginalFile
    } else if file.contains("document_persistence")
        || file.ends_with("/document_record_store.rs")
        || file.ends_with("/document_service.rs")
        || file.ends_with("/document_delete_service.rs")
        || file.ends_with("/document_tombstone_store.rs")
        || file.ends_with("/crdt_engine/workspace_ops.rs")
    {
        DirtyOrigin::DocumentPersistence
    } else if file.contains("semantic_") {
        DirtyOrigin::SemanticIndex
    } else if file.ends_with("/rdf_query_service.rs") {
        DirtyOrigin::RdfApi
    } else if file.ends_with("/rdf_seed_service.rs") {
        DirtyOrigin::RdfSeed
    } else if file.contains("rdf_record_materializer") || file.contains("geist_memory_rdf") {
        DirtyOrigin::RdfMaterializer
    } else if file.ends_with("/observatory/apply.rs") {
        DirtyOrigin::ObservatoryApply
    } else if file.contains("/emporium/") {
        DirtyOrigin::Emporium
    } else if file.contains("graph_") || file.ends_with("/document_paths.rs") {
        DirtyOrigin::GraphLifecycle
    } else {
        DirtyOrigin::Other
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KnownCrdtKind {
    CrdtFlush,
    DocumentWrite,
    DocumentEditComment,
    DocumentLiveProjection,
    DocumentIngestMarkdownOriginal,
    DocumentUploadIngest,
    DocumentBatchPrepare,
    DocumentBatchRegister,
    ImportVault,
    GraphImportArchive,
    ImportWebClip,
    BlockInsert,
    BlockUpdate,
    BlockEditText,
    BlockDelete,
    WorkspaceCreateDocument,
    WorkspaceUpdateDocument,
    WorkspaceDeleteDocument,
    WorkspaceCreateFolder,
    WorkspaceUpdateFolder,
    WorkspaceDeleteFolder,
    WorkspaceMoveFolder,
    WorkspaceMoveDocuments,
    WorkspacePutArtifact,
    WorkspaceDeleteArtifact,
    WorkspaceCreateWire,
    WorkspaceRefreshWire,
    WorkspaceDeleteWire,
}

impl KnownCrdtKind {
    const COUNT: usize = Self::WorkspaceDeleteWire as usize + 1;

    fn index(self) -> usize {
        self as usize
    }

    fn label(self) -> &'static str {
        match self {
            Self::CrdtFlush => "crdt.flush",
            Self::DocumentWrite => "document.write",
            Self::DocumentEditComment => "document.editComment",
            Self::DocumentLiveProjection => "document.liveProjection",
            Self::DocumentIngestMarkdownOriginal => "document.ingestMarkdownOriginal",
            Self::DocumentUploadIngest => "document.uploadIngest",
            Self::DocumentBatchPrepare => "document.batchPrepare",
            Self::DocumentBatchRegister => "document.batchRegister",
            Self::ImportVault => "import.vault",
            Self::GraphImportArchive => "graph.importArchive",
            Self::ImportWebClip => "import.webClip",
            Self::BlockInsert => "block.insert",
            Self::BlockUpdate => "block.update",
            Self::BlockEditText => "block.editText",
            Self::BlockDelete => "block.delete",
            Self::WorkspaceCreateDocument => "workspace.createDocument",
            Self::WorkspaceUpdateDocument => "workspace.updateDocument",
            Self::WorkspaceDeleteDocument => "workspace.deleteDocument",
            Self::WorkspaceCreateFolder => "workspace.createFolder",
            Self::WorkspaceUpdateFolder => "workspace.updateFolder",
            Self::WorkspaceDeleteFolder => "workspace.deleteFolder",
            Self::WorkspaceMoveFolder => "workspace.moveFolder",
            Self::WorkspaceMoveDocuments => "workspace.moveDocuments",
            Self::WorkspacePutArtifact => "workspace.putArtifact",
            Self::WorkspaceDeleteArtifact => "workspace.deleteArtifact",
            Self::WorkspaceCreateWire => "workspace.createWire",
            Self::WorkspaceRefreshWire => "workspace.refreshWire",
            Self::WorkspaceDeleteWire => "workspace.deleteWire",
        }
    }

    const ALL: [Self; Self::COUNT] = [
        Self::CrdtFlush,
        Self::DocumentWrite,
        Self::DocumentEditComment,
        Self::DocumentLiveProjection,
        Self::DocumentIngestMarkdownOriginal,
        Self::DocumentUploadIngest,
        Self::DocumentBatchPrepare,
        Self::DocumentBatchRegister,
        Self::ImportVault,
        Self::GraphImportArchive,
        Self::ImportWebClip,
        Self::BlockInsert,
        Self::BlockUpdate,
        Self::BlockEditText,
        Self::BlockDelete,
        Self::WorkspaceCreateDocument,
        Self::WorkspaceUpdateDocument,
        Self::WorkspaceDeleteDocument,
        Self::WorkspaceCreateFolder,
        Self::WorkspaceUpdateFolder,
        Self::WorkspaceDeleteFolder,
        Self::WorkspaceMoveFolder,
        Self::WorkspaceMoveDocuments,
        Self::WorkspacePutArtifact,
        Self::WorkspaceDeleteArtifact,
        Self::WorkspaceCreateWire,
        Self::WorkspaceRefreshWire,
        Self::WorkspaceDeleteWire,
    ];
}

pub(crate) fn known_crdt_kind(value: &str) -> Option<KnownCrdtKind> {
    Some(match value {
        "crdt.flush" => KnownCrdtKind::CrdtFlush,
        "document.write" => KnownCrdtKind::DocumentWrite,
        "document.editComment" => KnownCrdtKind::DocumentEditComment,
        "document.liveProjection" => KnownCrdtKind::DocumentLiveProjection,
        "document.ingestMarkdownOriginal" => KnownCrdtKind::DocumentIngestMarkdownOriginal,
        "document.uploadIngest" => KnownCrdtKind::DocumentUploadIngest,
        "document.batchPrepare" => KnownCrdtKind::DocumentBatchPrepare,
        "document.batchRegister" => KnownCrdtKind::DocumentBatchRegister,
        "import.vault" => KnownCrdtKind::ImportVault,
        "graph.importArchive" => KnownCrdtKind::GraphImportArchive,
        "import.webClip" => KnownCrdtKind::ImportWebClip,
        "block.insert" => KnownCrdtKind::BlockInsert,
        "block.update" => KnownCrdtKind::BlockUpdate,
        "block.editText" => KnownCrdtKind::BlockEditText,
        "block.delete" => KnownCrdtKind::BlockDelete,
        "workspace.createDocument" => KnownCrdtKind::WorkspaceCreateDocument,
        "workspace.updateDocument" => KnownCrdtKind::WorkspaceUpdateDocument,
        "workspace.deleteDocument" => KnownCrdtKind::WorkspaceDeleteDocument,
        "workspace.createFolder" => KnownCrdtKind::WorkspaceCreateFolder,
        "workspace.updateFolder" => KnownCrdtKind::WorkspaceUpdateFolder,
        "workspace.deleteFolder" => KnownCrdtKind::WorkspaceDeleteFolder,
        "workspace.moveFolder" => KnownCrdtKind::WorkspaceMoveFolder,
        "workspace.moveDocuments" => KnownCrdtKind::WorkspaceMoveDocuments,
        "workspace.putArtifact" => KnownCrdtKind::WorkspacePutArtifact,
        "workspace.deleteArtifact" => KnownCrdtKind::WorkspaceDeleteArtifact,
        "workspace.createWire" => KnownCrdtKind::WorkspaceCreateWire,
        "workspace.refreshWire" => KnownCrdtKind::WorkspaceRefreshWire,
        "workspace.deleteWire" => KnownCrdtKind::WorkspaceDeleteWire,
        _ => return None,
    })
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct EvidenceTotals {
    marks: u64,
    reasons: [u64; DirtyReason::COUNT],
    origins: [u64; DirtyOrigin::COUNT],
    crdt_kinds: [u64; KnownCrdtKind::COUNT],
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct DirtyEvidence {
    totals: EvidenceTotals,
    acknowledged: EvidenceTotals,
    saturated: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct DirtyEvidenceSnapshot {
    captured: EvidenceTotals,
    pending: EvidenceTotals,
    saturated: bool,
}

impl DirtyEvidence {
    pub(crate) fn record(
        &mut self,
        reason: DirtyReason,
        origin: DirtyOrigin,
        crdt_kind: Option<KnownCrdtKind>,
    ) {
        saturating_increment(&mut self.totals.marks, &mut self.saturated);
        saturating_increment(
            &mut self.totals.reasons[reason.index()],
            &mut self.saturated,
        );
        saturating_increment(
            &mut self.totals.origins[origin.index()],
            &mut self.saturated,
        );
        if let Some(kind) = crdt_kind {
            saturating_increment(
                &mut self.totals.crdt_kinds[kind.index()],
                &mut self.saturated,
            );
        }
    }

    pub(crate) fn pending_snapshot(&self) -> DirtyEvidenceSnapshot {
        DirtyEvidenceSnapshot {
            captured: self.totals,
            pending: subtract_totals(self.totals, self.acknowledged),
            saturated: self.saturated,
        }
    }

    pub(crate) fn acknowledge(&mut self, snapshot: &DirtyEvidenceSnapshot) {
        self.acknowledged.marks = self.acknowledged.marks.max(snapshot.captured.marks);
        for index in 0..DirtyReason::COUNT {
            self.acknowledged.reasons[index] =
                self.acknowledged.reasons[index].max(snapshot.captured.reasons[index]);
        }
        for index in 0..DirtyOrigin::COUNT {
            self.acknowledged.origins[index] =
                self.acknowledged.origins[index].max(snapshot.captured.origins[index]);
        }
        for index in 0..KnownCrdtKind::COUNT {
            self.acknowledged.crdt_kinds[index] =
                self.acknowledged.crdt_kinds[index].max(snapshot.captured.crdt_kinds[index]);
        }
    }
}

fn saturating_increment(value: &mut u64, saturated: &mut bool) {
    if *value == u64::MAX {
        *saturated = true;
    } else {
        *value += 1;
    }
}

fn subtract_totals(total: EvidenceTotals, acknowledged: EvidenceTotals) -> EvidenceTotals {
    let mut pending = EvidenceTotals {
        marks: total.marks.saturating_sub(acknowledged.marks),
        ..EvidenceTotals::default()
    };
    for index in 0..DirtyReason::COUNT {
        pending.reasons[index] = total.reasons[index].saturating_sub(acknowledged.reasons[index]);
    }
    for index in 0..DirtyOrigin::COUNT {
        pending.origins[index] = total.origins[index].saturating_sub(acknowledged.origins[index]);
    }
    for index in 0..KnownCrdtKind::COUNT {
        pending.crdt_kinds[index] =
            total.crdt_kinds[index].saturating_sub(acknowledged.crdt_kinds[index]);
    }
    pending
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StoreKind {
    Graph,
    ProfileMetadata,
    Omphalos,
    Other,
}

impl StoreKind {
    pub(crate) fn classify(profile_dir: &Path, store_path: &Path) -> Self {
        let name = store_path.file_name().and_then(|name| name.to_str());
        if name == Some("metadata.oxigraph") {
            Self::ProfileMetadata
        } else if name == Some("omphalos") {
            Self::Omphalos
        } else if name == Some("store.oxigraph")
            && store_path
                .strip_prefix(profile_dir)
                .ok()
                .is_some_and(|relative| relative.starts_with("graphs"))
        {
            Self::Graph
        } else {
            Self::Other
        }
    }
}

pub(crate) fn next_store_slot_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StoreDecision {
    Backup,
    ReuseClean,
}

#[derive(Clone, Debug)]
pub(crate) struct StoreObservation {
    pub(crate) slot_id: u64,
    pub(crate) incarnation: u64,
    pub(crate) store_kind: StoreKind,
    pub(crate) decision: StoreDecision,
    pub(crate) dirty_epoch: u64,
    pub(crate) backed_up_epoch_before: u64,
    pub(crate) backed_up_epoch_after: u64,
    pub(crate) backup_ms: u64,
    pub(crate) evidence: DirtyEvidenceSnapshot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TraceOutcome {
    Published,
    ConfirmedClean,
    Deferred,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DeferReason {
    ImportActive,
    FlushGateContended,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FailedPhase {
    Serialize,
    Prepare,
    StoreLifecycleGate,
    FlushGateWait,
    StoreBackup,
    Walk,
    Decision,
    Publish,
    Prune,
    Cleanup,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TimingPhase {
    SerialWait,
    StoreLifecycleWait,
    FlushGateWait,
    FlushGateHeld,
    StoreBackup,
    Walk,
    Decision,
    Publish,
    Prune,
}

#[derive(Default)]
struct TimingCells {
    serial_wait_ms: Cell<u64>,
    store_lifecycle_wait_ms: Cell<u64>,
    flush_gate_wait_ms: Cell<u64>,
    flush_gate_held_ms: Cell<u64>,
    store_backup_ms: Cell<u64>,
    walk_ms: Cell<u64>,
    decision_ms: Cell<u64>,
    publish_ms: Cell<u64>,
    prune_ms: Cell<u64>,
}

pub(crate) struct PhaseTimer<'a> {
    started: Option<Instant>,
    target: &'a Cell<u64>,
}

impl Drop for PhaseTimer<'_> {
    fn drop(&mut self) {
        let Some(started) = self.started else {
            return;
        };
        self.target.set(
            self.target
                .get()
                .saturating_add(duration_ms(started.elapsed())),
        );
    }
}

pub(crate) struct FlushTrace {
    enabled: bool,
    trigger: FlushTrigger,
    forced: bool,
    attempt_seq: u64,
    started: Instant,
    epoch_at_start: u64,
    current_phase: Cell<FailedPhase>,
    defer_reason: Cell<Option<DeferReason>>,
    previous_sequence: Cell<Option<u64>>,
    snapshot_sequence: Cell<Option<u64>>,
    global_evidence: RefCell<Option<DirtyEvidenceSnapshot>>,
    stores: RefCell<Vec<StoreObservation>>,
    stores_omitted: Cell<usize>,
    files_copied: Cell<u64>,
    files_linked: Cell<u64>,
    stores_backed_up: Cell<u64>,
    bytes_copied: Cell<u64>,
    timings: TimingCells,
}

impl FlushTrace {
    pub(crate) fn new(trigger: FlushTrigger, forced: bool, epoch_at_start: u64) -> Self {
        Self::new_with_enabled(trigger, forced, epoch_at_start, tracing_enabled())
    }

    fn new_with_enabled(
        trigger: FlushTrigger,
        forced: bool,
        epoch_at_start: u64,
        enabled: bool,
    ) -> Self {
        static NEXT_ATTEMPT: AtomicU64 = AtomicU64::new(1);
        Self {
            enabled,
            trigger,
            forced,
            attempt_seq: if enabled {
                NEXT_ATTEMPT.fetch_add(1, Ordering::Relaxed)
            } else {
                0
            },
            started: Instant::now(),
            epoch_at_start,
            current_phase: Cell::new(FailedPhase::Serialize),
            defer_reason: Cell::new(None),
            previous_sequence: Cell::new(None),
            snapshot_sequence: Cell::new(None),
            global_evidence: RefCell::new(None),
            stores: RefCell::new(Vec::new()),
            stores_omitted: Cell::new(0),
            files_copied: Cell::new(0),
            files_linked: Cell::new(0),
            stores_backed_up: Cell::new(0),
            bytes_copied: Cell::new(0),
            timings: TimingCells::default(),
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    pub(crate) fn set_phase(&self, phase: FailedPhase) {
        if self.enabled {
            self.current_phase.set(phase);
        }
    }

    pub(crate) fn set_defer_reason(&self, reason: DeferReason) {
        if self.enabled {
            self.defer_reason.set(Some(reason));
        }
    }

    pub(crate) fn set_previous_sequence(&self, sequence: Option<u64>) {
        if self.enabled {
            self.previous_sequence.set(sequence);
        }
    }

    pub(crate) fn set_snapshot_sequence(&self, sequence: u64) {
        if self.enabled {
            self.snapshot_sequence.set(Some(sequence));
        }
    }

    pub(crate) fn set_global_evidence(&self, evidence: DirtyEvidenceSnapshot) {
        if self.enabled {
            *self.global_evidence.borrow_mut() = Some(evidence);
        }
    }

    pub(crate) fn record_store(&self, observation: StoreObservation) {
        if !self.enabled {
            return;
        }
        let mut stores = self.stores.borrow_mut();
        if stores.len() < MAX_STORE_DETAILS {
            stores.push(observation);
        } else {
            self.stores_omitted
                .set(self.stores_omitted.get().saturating_add(1));
        }
    }

    pub(crate) fn complete_store_backup(&self, slot_id: u64, incarnation: u64, backup_ms: u64) {
        if !self.enabled {
            return;
        }
        if let Some(store) = self
            .stores
            .borrow_mut()
            .iter_mut()
            .find(|store| store.slot_id == slot_id && store.incarnation == incarnation)
        {
            store.backup_ms = backup_ms;
        }
    }

    pub(crate) fn acknowledge_store(
        &self,
        slot_id: u64,
        incarnation: u64,
        backed_up_epoch_after: u64,
    ) {
        if !self.enabled {
            return;
        }
        if let Some(store) = self
            .stores
            .borrow_mut()
            .iter_mut()
            .find(|store| store.slot_id == slot_id && store.incarnation == incarnation)
        {
            store.backed_up_epoch_after = backed_up_epoch_after;
        }
    }

    pub(crate) fn observe_io(
        &self,
        files_copied: usize,
        files_linked: usize,
        stores_backed_up: usize,
        bytes_copied: u64,
    ) {
        if !self.enabled {
            return;
        }
        self.files_copied
            .set(u64::try_from(files_copied).unwrap_or(u64::MAX));
        self.files_linked
            .set(u64::try_from(files_linked).unwrap_or(u64::MAX));
        self.stores_backed_up
            .set(u64::try_from(stores_backed_up).unwrap_or(u64::MAX));
        self.bytes_copied.set(bytes_copied);
    }

    pub(crate) fn timer(&self, phase: TimingPhase) -> PhaseTimer<'_> {
        let target = match phase {
            TimingPhase::SerialWait => &self.timings.serial_wait_ms,
            TimingPhase::StoreLifecycleWait => &self.timings.store_lifecycle_wait_ms,
            TimingPhase::FlushGateWait => &self.timings.flush_gate_wait_ms,
            TimingPhase::FlushGateHeld => &self.timings.flush_gate_held_ms,
            TimingPhase::StoreBackup => &self.timings.store_backup_ms,
            TimingPhase::Walk => &self.timings.walk_ms,
            TimingPhase::Decision => &self.timings.decision_ms,
            TimingPhase::Publish => &self.timings.publish_ms,
            TimingPhase::Prune => &self.timings.prune_ms,
        };
        PhaseTimer {
            started: self.enabled.then(Instant::now),
            target,
        }
    }

    pub(crate) fn finish(&self, outcome: TraceOutcome, epoch_at_finish: u64, dirty_unknown: bool) {
        if !self.enabled {
            return;
        }
        let event = self.build_event(outcome, epoch_at_finish, dirty_unknown);
        let line = bounded_json(event);
        log::info!(target: "garden::durability_causality", "durable_flush_trace {line}");
    }

    fn build_event(
        &self,
        outcome: TraceOutcome,
        epoch_at_finish: u64,
        dirty_unknown: bool,
    ) -> TraceEvent {
        let global = self
            .global_evidence
            .borrow()
            .clone()
            .unwrap_or_else(empty_snapshot);
        TraceEvent {
            schema: TRACE_SCHEMA,
            event: TRACE_EVENT,
            machine_run_id: machine_run_id(),
            attempt_seq: self.attempt_seq,
            trigger: self.trigger,
            forced: self.forced,
            outcome,
            defer_reason: self.defer_reason.get(),
            failed_phase: (outcome == TraceOutcome::Failed).then(|| self.current_phase.get()),
            previous_sequence: self.previous_sequence.get(),
            snapshot_sequence: self.snapshot_sequence.get(),
            epoch_at_start: self.epoch_at_start,
            epoch_at_finish,
            dirty_unknown,
            files_copied: self.files_copied.get(),
            files_linked: self.files_linked.get(),
            stores_backed_up: self.stores_backed_up.get(),
            bytes_copied: self.bytes_copied.get(),
            dirty_signals: evidence_summary(&global),
            stores: self.stores.borrow().iter().map(store_event).collect(),
            stores_omitted: u64::try_from(self.stores_omitted.get()).unwrap_or(u64::MAX),
            timing_ms: TimingEvent {
                serial_wait: self.timings.serial_wait_ms.get(),
                store_lifecycle_wait: self.timings.store_lifecycle_wait_ms.get(),
                flush_gate_wait: self.timings.flush_gate_wait_ms.get(),
                flush_gate_held: self.timings.flush_gate_held_ms.get(),
                store_backup: self.timings.store_backup_ms.get(),
                walk: self.timings.walk_ms.get(),
                decision: self.timings.decision_ms.get(),
                publish: self.timings.publish_ms.get(),
                prune: self.timings.prune_ms.get(),
                total: duration_ms(self.started.elapsed()),
            },
        }
    }

    #[cfg(test)]
    fn render_for_test(&self, outcome: TraceOutcome) -> String {
        self.observe_io(1, 2, 1, 3);
        self.set_snapshot_sequence(1);
        bounded_json(self.build_event(outcome, 2, false))
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

pub(crate) fn elapsed_ms(started: Instant) -> u64 {
    duration_ms(started.elapsed())
}

fn machine_run_id() -> Option<String> {
    std::env::var("GARDEN_CELL_MACHINE_RUN_ID")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| {
            value.len() == 26
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        })
}

fn empty_snapshot() -> DirtyEvidenceSnapshot {
    DirtyEvidenceSnapshot::default()
}

#[derive(Serialize)]
struct NamedCount {
    name: &'static str,
    count: u64,
}

#[derive(Serialize)]
struct EvidenceEvent {
    marks: u64,
    saturated: bool,
    reasons: Vec<NamedCount>,
    origins: Vec<NamedCount>,
    crdt_kinds: Vec<NamedCount>,
    counts_omitted: u64,
}

fn evidence_summary(snapshot: &DirtyEvidenceSnapshot) -> EvidenceEvent {
    let mut omitted = 0u64;
    let reasons = sparse_counts(
        &DirtyReason::ALL,
        &snapshot.pending.reasons,
        DirtyReason::label,
        &mut omitted,
    );
    let origins = sparse_counts(
        &DirtyOrigin::ALL,
        &snapshot.pending.origins,
        DirtyOrigin::label,
        &mut omitted,
    );
    let crdt_kinds = sparse_counts(
        &KnownCrdtKind::ALL,
        &snapshot.pending.crdt_kinds,
        KnownCrdtKind::label,
        &mut omitted,
    );
    EvidenceEvent {
        marks: snapshot.pending.marks,
        saturated: snapshot.saturated,
        reasons,
        origins,
        crdt_kinds,
        counts_omitted: omitted,
    }
}

fn sparse_counts<T: Copy>(
    values: &[T],
    counts: &[u64],
    label: fn(T) -> &'static str,
    omitted: &mut u64,
) -> Vec<NamedCount> {
    let mut nonzero = values
        .iter()
        .copied()
        .zip(counts.iter().copied())
        .filter(|(_, count)| *count > 0)
        .collect::<Vec<_>>();
    nonzero.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    if nonzero.len() > MAX_SPARSE_COUNTS {
        *omitted = omitted.saturating_add((nonzero.len() - MAX_SPARSE_COUNTS) as u64);
        nonzero.truncate(MAX_SPARSE_COUNTS);
    }
    nonzero
        .into_iter()
        .map(|(value, count)| NamedCount {
            name: label(value),
            count,
        })
        .collect()
}

#[derive(Serialize)]
struct StoreEvent {
    slot_id: u64,
    incarnation: u64,
    store_kind: StoreKind,
    decision: StoreDecision,
    dirty_epoch: u64,
    backed_up_epoch_before: u64,
    backed_up_epoch_after: u64,
    backup_ms: u64,
    dirty_signals: EvidenceEvent,
}

fn store_event(observation: &StoreObservation) -> StoreEvent {
    StoreEvent {
        slot_id: observation.slot_id,
        incarnation: observation.incarnation,
        store_kind: observation.store_kind,
        decision: observation.decision,
        dirty_epoch: observation.dirty_epoch,
        backed_up_epoch_before: observation.backed_up_epoch_before,
        backed_up_epoch_after: observation.backed_up_epoch_after,
        backup_ms: observation.backup_ms,
        dirty_signals: evidence_summary(&observation.evidence),
    }
}

#[derive(Serialize)]
struct TimingEvent {
    serial_wait: u64,
    store_lifecycle_wait: u64,
    flush_gate_wait: u64,
    flush_gate_held: u64,
    store_backup: u64,
    walk: u64,
    decision: u64,
    publish: u64,
    prune: u64,
    total: u64,
}

#[derive(Serialize)]
struct TraceEvent {
    schema: &'static str,
    event: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    machine_run_id: Option<String>,
    attempt_seq: u64,
    trigger: FlushTrigger,
    forced: bool,
    outcome: TraceOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    defer_reason: Option<DeferReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failed_phase: Option<FailedPhase>,
    #[serde(skip_serializing_if = "Option::is_none")]
    previous_sequence: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    snapshot_sequence: Option<u64>,
    epoch_at_start: u64,
    epoch_at_finish: u64,
    dirty_unknown: bool,
    files_copied: u64,
    files_linked: u64,
    stores_backed_up: u64,
    bytes_copied: u64,
    dirty_signals: EvidenceEvent,
    stores: Vec<StoreEvent>,
    stores_omitted: u64,
    timing_ms: TimingEvent,
}

fn bounded_json(mut event: TraceEvent) -> String {
    let mut line = serde_json::to_string(&event).unwrap_or_else(|_| {
        "{\"schema\":\"garden.flush_dirty_causality.v1\",\"event\":\"serialization_failed\"}"
            .to_string()
    });
    if line.len() <= MAX_EVENT_BYTES {
        return line;
    }
    event.stores_omitted = event
        .stores_omitted
        .saturating_add(u64::try_from(event.stores.len()).unwrap_or(u64::MAX));
    event.stores.clear();
    line = serde_json::to_string(&event).unwrap_or_else(|_| {
        "{\"schema\":\"garden.flush_dirty_causality.v1\",\"event\":\"serialization_failed\"}"
            .to_string()
    });
    if line.len() <= MAX_EVENT_BYTES {
        line
    } else {
        format!(
            "{{\"schema\":\"{TRACE_SCHEMA}\",\"event\":\"{TRACE_EVENT}\",\"attempt_seq\":{},\"truncated\":true}}",
            event.attempt_seq
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_acknowledges_only_the_captured_counts() {
        let mut evidence = DirtyEvidence::default();
        evidence.record(
            DirtyReason::GraphLeaseFallback,
            DirtyOrigin::CrdtApply,
            known_crdt_kind("crdt.flush"),
        );
        let captured = evidence.pending_snapshot();
        evidence.record(
            DirtyReason::RdfDirectHook,
            DirtyOrigin::ObservatoryApply,
            None,
        );
        evidence.acknowledge(&captured);

        let pending = evidence.pending_snapshot();
        assert_eq!(pending.pending.marks, 1);
        assert_eq!(
            pending.pending.reasons[DirtyReason::RdfDirectHook.index()],
            1
        );
        assert_eq!(
            pending.pending.origins[DirtyOrigin::ObservatoryApply.index()],
            1
        );
        assert_eq!(
            pending.pending.crdt_kinds[KnownCrdtKind::CrdtFlush.index()],
            0
        );
    }

    #[test]
    fn crdt_kind_domain_is_closed() {
        assert_eq!(
            known_crdt_kind("workspace.refreshWire"),
            Some(KnownCrdtKind::WorkspaceRefreshWire)
        );
        assert_eq!(known_crdt_kind("attacker.supplied-kind"), None);
    }

    #[test]
    fn source_paths_collapse_to_fixed_origins() {
        assert_eq!(
            origin_from_file("src/local_job_db.rs"),
            DirtyOrigin::LocalJobDb
        );
        assert_eq!(
            origin_from_file("src/crdt_engine/executor.rs"),
            DirtyOrigin::CrdtApply
        );
        assert_eq!(
            origin_from_file("src/a-new-unclassified-writer.rs"),
            DirtyOrigin::Other
        );
    }

    #[test]
    fn store_kind_never_requires_serializing_a_path() {
        let profile = Path::new("/profile");
        assert_eq!(
            StoreKind::classify(profile, Path::new("/profile/graphs/g/store.oxigraph")),
            StoreKind::Graph
        );
        assert_eq!(
            StoreKind::classify(profile, Path::new("/profile/metadata.oxigraph")),
            StoreKind::ProfileMetadata
        );
        assert_eq!(
            StoreKind::classify(profile, Path::new("/profile/omphalos")),
            StoreKind::Omphalos
        );
    }

    #[test]
    fn bounded_event_contains_no_raw_path_or_arbitrary_kind() {
        let trace = FlushTrace::new_with_enabled(FlushTrigger::PeriodicDebounce, false, 1, true);
        let mut evidence = DirtyEvidence::default();
        evidence.record(
            DirtyReason::WriteGuardCompleted,
            DirtyOrigin::LocalJobDb,
            None,
        );
        trace.set_global_evidence(evidence.pending_snapshot());
        for slot_id in 1..=10 {
            trace.record_store(StoreObservation {
                slot_id,
                incarnation: 1,
                store_kind: StoreKind::Graph,
                decision: StoreDecision::Backup,
                dirty_epoch: 2,
                backed_up_epoch_before: 1,
                backed_up_epoch_after: 2,
                backup_ms: 3,
                evidence: evidence.pending_snapshot(),
            });
        }
        let line = trace.render_for_test(TraceOutcome::Published);
        assert!(line.len() <= MAX_EVENT_BYTES);
        assert!(!line.contains("/profile"));
        assert!(!line.contains("attacker.supplied-kind"));
        assert!(line.contains("local_job_db"));
        assert!(line.contains("stores_omitted"));
        assert!(line.contains("\"files_linked\":2"));
    }

    #[test]
    fn failed_phase_is_present_only_on_failure() {
        let failed = FlushTrace::new_with_enabled(FlushTrigger::ManualForced, true, 1, true);
        failed.set_phase(FailedPhase::StoreBackup);
        let failed_line = failed.render_for_test(TraceOutcome::Failed);
        assert!(failed_line.contains("\"failed_phase\":\"store_backup\""));

        let success = FlushTrace::new_with_enabled(FlushTrigger::ManualForced, true, 1, true);
        success.set_phase(FailedPhase::StoreBackup);
        let success_line = success.render_for_test(TraceOutcome::Published);
        assert!(!success_line.contains("failed_phase"));
    }

    #[test]
    fn trigger_label_is_not_the_force_authority() {
        let trace = FlushTrace::new_with_enabled(FlushTrigger::PeriodicDebounce, true, 1, true);
        let line = trace.render_for_test(TraceOutcome::Published);
        assert!(line.contains("\"trigger\":\"periodic_debounce\""));
        assert!(line.contains("\"forced\":true"));
    }

    #[test]
    fn disabled_trace_does_not_allocate_an_attempt_sequence() {
        let trace = FlushTrace::new_with_enabled(FlushTrigger::PeriodicDebounce, false, 1, false);
        assert_eq!(trace.attempt_seq, 0);
    }
}
