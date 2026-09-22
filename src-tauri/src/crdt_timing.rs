use crate::{clock::timestamp, crdt_operation_types::CrdtOperation};
use std::{collections::BTreeMap, time::Instant};

pub(super) struct CrdtOperationTimingSeed {
    operation: CrdtOperation,
    enqueued_at: Instant,
    enqueued_at_wall: String,
    emitted_at: Option<Instant>,
    emit_ms: Option<f64>,
    polled_at: Option<Instant>,
    polled_at_wall: Option<String>,
    phases: BTreeMap<String, f64>,
}

impl CrdtOperationTimingSeed {
    pub(super) fn new(operation: CrdtOperation) -> Self {
        Self {
            operation,
            enqueued_at: Instant::now(),
            enqueued_at_wall: timestamp(),
            emitted_at: None,
            emit_ms: None,
            polled_at: None,
            polled_at_wall: None,
            phases: BTreeMap::new(),
        }
    }

    pub(super) fn mark_emitted(&mut self, emit_ms: f64) {
        self.emitted_at = Some(Instant::now());
        self.emit_ms = Some(emit_ms);
    }

    pub(super) fn mark_polled(&mut self) {
        self.polled_at = Some(Instant::now());
        self.polled_at_wall = Some(timestamp());
    }

    pub(super) fn add_phase(&mut self, phase: &str, elapsed_ms: f64) {
        self.phases.insert(phase.to_string(), elapsed_ms);
    }

    pub(super) fn completion_trace(
        self,
        ok: bool,
        error: Option<String>,
        timing: Option<serde_json::Value>,
    ) -> serde_json::Value {
        let completed_at = Instant::now();
        let completed_at_wall = timestamp();
        let mut phases = serde_json::Map::new();
        phases.insert(
            "rustEnqueueToCompleteMs".to_string(),
            serde_json::json!(crate::clock::duration_ms(
                completed_at.duration_since(self.enqueued_at)
            )),
        );
        if let Some(emit_ms) = self.emit_ms {
            phases.insert("rustEmitMs".to_string(), serde_json::json!(emit_ms));
        }
        if let Some(emitted_at) = self.emitted_at {
            phases.insert(
                "rustEmitToCompleteMs".to_string(),
                serde_json::json!(crate::clock::duration_ms(
                    completed_at.duration_since(emitted_at)
                )),
            );
        }
        if let Some(polled_at) = self.polled_at {
            phases.insert(
                "rustQueueToPollMs".to_string(),
                serde_json::json!(crate::clock::duration_ms(
                    polled_at.duration_since(self.enqueued_at)
                )),
            );
            phases.insert(
                "rustPollToCompleteMs".to_string(),
                serde_json::json!(crate::clock::duration_ms(
                    completed_at.duration_since(polled_at)
                )),
            );
        }
        for (phase, elapsed_ms) in self.phases {
            phases.insert(phase, serde_json::json!(elapsed_ms));
        }
        if let Some(desktop_timing) = timing.as_ref() {
            if let Some(total_ms) = desktop_timing.get("totalMs") {
                phases.insert("jsTotalMs".to_string(), total_ms.clone());
            }
            if let Some(desktop_phases) = desktop_timing
                .get("phases")
                .and_then(serde_json::Value::as_object)
            {
                for (phase, elapsed_ms) in desktop_phases {
                    phases.insert(format!("js{phase}"), elapsed_ms.clone());
                }
            }
        }

        serde_json::json!({
            "operationId": self.operation.operation_id,
            "kind": self.operation.kind,
            "graphId": self.operation.graph_id,
            "documentId": self.operation.document_id,
            "ok": ok,
            "error": error,
            "enqueuedAt": self.enqueued_at_wall,
            "polledAt": self.polled_at_wall,
            "completedAt": completed_at_wall,
            "phases": serde_json::Value::Object(phases),
            "desktopTiming": timing,
        })
    }
}
