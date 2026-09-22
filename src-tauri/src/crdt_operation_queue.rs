use crate::{
    crdt_operation_types::{CompleteCrdtOperationInput, CrdtOperation, CrdtOperationResult},
    crdt_timing::CrdtOperationTimingSeed,
};
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    sync::Mutex,
    time::Duration,
};
use tokio::sync::oneshot;
use tokio::time::Instant;

const ATTACHED_RETRY_BACKOFF_BASE_MS: u64 = 25;
const ATTACHED_RETRY_BACKOFF_MAX_MS: u64 = 1_000;
const DETACHED_RETRY_BACKOFF_BASE_MS: u64 = 1_000;
const DETACHED_RETRY_BACKOFF_MAX_MS: u64 = 60_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RetrySchedule {
    pub(crate) attempt: u32,
    pub(crate) delay: Duration,
    pub(crate) has_live_responder: bool,
    operation_id: String,
    not_before: Instant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RetryWait {
    attempt: u32,
    not_before: Instant,
}

#[derive(Default)]
struct CrdtOperationQueueInner {
    pending: VecDeque<CrdtOperation>,
    in_flight: BTreeMap<String, CrdtOperation>,
    responders: BTreeMap<String, oneshot::Sender<CrdtOperationResult>>,
    retry_attempts: BTreeMap<String, u32>,
    retry_waits: BTreeMap<String, RetryWait>,
    timing: BTreeMap<String, CrdtOperationTimingSeed>,
    traces: VecDeque<serde_json::Value>,
}

#[derive(Default)]
pub(crate) struct CrdtOperationQueue {
    inner: Mutex<CrdtOperationQueueInner>,
    /// Headless enqueues each spawn a best-effort drainer. Only one may poll
    /// and apply at a time, otherwise a later drainer can move op2 in-flight
    /// and win the graph lease before the drainer that already polled op1.
    drain: tokio::sync::Mutex<()>,
}

impl CrdtOperationQueue {
    pub(crate) async fn lock_drain(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.drain.lock().await
    }

    pub(super) fn enqueue(
        &self,
        operation: CrdtOperation,
    ) -> Result<oneshot::Receiver<CrdtOperationResult>, String> {
        let (sender, receiver) = oneshot::channel();
        self.enqueue_inner(operation, Some(sender))?;
        Ok(receiver)
    }

    pub(crate) fn enqueue_detached(&self, operation: CrdtOperation) -> Result<(), String> {
        self.enqueue_inner(operation, None)
    }

    fn enqueue_inner(
        &self,
        operation: CrdtOperation,
        responder: Option<oneshot::Sender<CrdtOperationResult>>,
    ) -> Result<(), String> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "CRDT operation queue lock poisoned".to_string())?;
        if inner
            .pending
            .iter()
            .any(|pending| pending.operation_id == operation.operation_id)
            || inner.in_flight.contains_key(&operation.operation_id)
        {
            if let Some(responder) = responder {
                inner
                    .responders
                    .insert(operation.operation_id.clone(), responder);
            }
            return Ok(());
        }
        if let Some(responder) = responder {
            inner
                .responders
                .insert(operation.operation_id.clone(), responder);
        }
        inner.pending.push_back(operation);
        let queued_operation = inner.pending.back().cloned();
        if let Some(queued_operation) = queued_operation {
            inner.timing.insert(
                queued_operation.operation_id.clone(),
                CrdtOperationTimingSeed::new(queued_operation),
            );
        }
        Ok(())
    }

    pub(crate) fn poll(&self, limit: Option<usize>) -> Result<Vec<CrdtOperation>, String> {
        let limit = limit.unwrap_or(20).clamp(1, 100);
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "CRDT operation queue lock poisoned".to_string())?;
        let mut operations = Vec::new();
        // Preserve FIFO within each graph while allowing unrelated graphs to
        // make progress. A scheduled retry blocks only later operations for
        // the same graph. Scanning from the front still chooses the oldest
        // ready operation across unblocked graphs. The production drainer
        // polls one operation at a time under its single-flight mutex.
        let mut blocked_graphs = HashSet::<String>::new();
        let mut index = 0;
        while operations.len() < limit && index < inner.pending.len() {
            let candidate = &inner.pending[index];
            if blocked_graphs.contains(&candidate.graph_id) {
                index += 1;
                continue;
            }
            if inner.retry_waits.contains_key(&candidate.operation_id) {
                blocked_graphs.insert(candidate.graph_id.clone());
                index += 1;
                continue;
            }
            let Some(operation) = inner.pending.remove(index) else {
                break;
            };
            if let Some(seed) = inner.timing.get_mut(&operation.operation_id) {
                seed.mark_polled();
            }
            inner
                .in_flight
                .insert(operation.operation_id.clone(), operation.clone());
            operations.push(operation);
        }
        Ok(operations)
    }

    pub(super) fn mark_emitted(&self, operation_id: &str, emit_ms: f64) {
        if let Ok(mut inner) = self.inner.lock() {
            if let Some(seed) = inner.timing.get_mut(operation_id) {
                seed.mark_emitted(emit_ms);
            }
        }
    }

    pub(crate) fn add_phase(
        &self,
        operation_id: &str,
        phase: &str,
        elapsed_ms: f64,
    ) -> Result<(), String> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "CRDT operation queue lock poisoned".to_string())?;

        if let Some(seed) = inner.timing.get_mut(operation_id) {
            seed.add_phase(phase, elapsed_ms);
            return Ok(());
        }

        for trace in inner.traces.iter_mut().rev() {
            if trace.get("operationId").and_then(serde_json::Value::as_str) != Some(operation_id) {
                continue;
            }
            let Some(trace_object) = trace.as_object_mut() else {
                return Ok(());
            };
            let phases = trace_object
                .entry("phases".to_string())
                .or_insert_with(|| serde_json::json!({}));
            if let Some(phases_object) = phases.as_object_mut() {
                phases_object.insert(phase.to_string(), serde_json::json!(elapsed_ms));
            }
            return Ok(());
        }

        Ok(())
    }

    pub(crate) fn recent_traces(
        &self,
        limit: Option<usize>,
        clear: bool,
        kind: Option<&str>,
        document_id: Option<&str>,
        operation_id: Option<&str>,
    ) -> Result<Vec<serde_json::Value>, String> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "CRDT operation queue lock poisoned".to_string())?;
        let limit = limit.unwrap_or(20).clamp(1, 200);
        let mut traces = inner
            .traces
            .iter()
            .rev()
            .filter(|trace| {
                kind.is_none_or(|expected| {
                    trace.get("kind").and_then(serde_json::Value::as_str) == Some(expected)
                }) && document_id.is_none_or(|expected| {
                    trace.get("documentId").and_then(serde_json::Value::as_str) == Some(expected)
                }) && operation_id.is_none_or(|expected| {
                    trace.get("operationId").and_then(serde_json::Value::as_str) == Some(expected)
                })
            })
            .take(limit)
            .cloned()
            .collect::<Vec<_>>();
        traces.reverse();
        if clear {
            inner.traces.clear();
        }
        Ok(traces)
    }

    pub(super) fn complete(&self, input: CompleteCrdtOperationInput) -> Result<(), String> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "CRDT operation queue lock poisoned".to_string())?;
        let operation_id = input.operation_id.clone();
        let error = input.error.clone();
        let timing = input.timing.clone();
        inner.in_flight.remove(&operation_id);
        inner.retry_attempts.remove(&operation_id);
        inner.retry_waits.remove(&operation_id);
        let seed = inner.timing.remove(&operation_id);
        if let Some(seed) = seed {
            inner
                .traces
                .push_back(seed.completion_trace(input.ok, error, timing));
            while inner.traces.len() > 200 {
                inner.traces.pop_front();
            }
        }
        if let Some(sender) = inner.responders.remove(&input.operation_id) {
            let _ = sender.send(CrdtOperationResult {
                ok: input.ok,
                value: input.value,
                error: input.error,
                timing: input.timing,
            });
        }
        Ok(())
    }

    /// Retain a post-hot failure under its original operation ID and install a
    /// not-before barrier at the FIFO head. The caller responder and timing
    /// seed stay attached when the receiver is still live. The production
    /// drainer schedules exactly one timer from the returned token, then
    /// releases its single-flight guard immediately.
    pub(crate) fn requeue_retryable_after_hot_commit(
        &self,
        operation: CrdtOperation,
    ) -> Result<RetrySchedule, String> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "CRDT operation queue lock poisoned".to_string())?;
        let operation_id = operation.operation_id.clone();
        let Some(in_flight) = inner.in_flight.remove(&operation_id) else {
            return Err(format!(
                "retryable CRDT operation is not in flight: {operation_id}"
            ));
        };
        if in_flight.kind != operation.kind
            || in_flight.graph_id != operation.graph_id
            || in_flight.document_id != operation.document_id
        {
            inner.in_flight.insert(operation_id.clone(), in_flight);
            return Err(format!(
                "retryable CRDT operation identity changed while in flight: {operation_id}"
            ));
        }
        if inner
            .pending
            .iter()
            .any(|pending| pending.operation_id == operation_id)
        {
            inner.in_flight.insert(operation_id.clone(), in_flight);
            return Err(format!(
                "retryable CRDT operation is already pending: {operation_id}"
            ));
        }
        if inner.retry_waits.contains_key(&operation_id) {
            inner.in_flight.insert(operation_id.clone(), in_flight);
            return Err(format!(
                "retryable CRDT operation already has a scheduled wait: {operation_id}"
            ));
        }
        inner.pending.push_front(operation);
        let attempt = inner
            .retry_attempts
            .entry(operation_id.clone())
            .and_modify(|attempt| *attempt = attempt.saturating_add(1))
            .or_insert(1);
        let attempt = *attempt;
        if inner
            .responders
            .get(&operation_id)
            .is_some_and(oneshot::Sender::is_closed)
        {
            // Dropping an HTTP future drops its oneshot receiver without
            // calling the explicit timeout detach path. Never mistake that
            // stale Sender entry for a live caller.
            inner.responders.remove(&operation_id);
        }
        let has_live_responder = inner.responders.contains_key(&operation_id);
        let (base_ms, max_ms) = if has_live_responder {
            (
                ATTACHED_RETRY_BACKOFF_BASE_MS,
                ATTACHED_RETRY_BACKOFF_MAX_MS,
            )
        } else {
            (
                DETACHED_RETRY_BACKOFF_BASE_MS,
                DETACHED_RETRY_BACKOFF_MAX_MS,
            )
        };
        let shift = attempt.saturating_sub(1).min(31);
        let multiplier = 1_u64.checked_shl(shift).unwrap_or(u64::MAX);
        let delay_ms = base_ms.saturating_mul(multiplier).min(max_ms);
        let delay = Duration::from_millis(delay_ms);
        let not_before = Instant::now() + delay;
        inner.retry_waits.insert(
            operation_id.clone(),
            RetryWait {
                attempt,
                not_before,
            },
        );
        Ok(RetrySchedule {
            attempt,
            delay,
            has_live_responder,
            operation_id,
            not_before,
        })
    }

    /// Sleep until this exact schedule is due, then atomically remove its
    /// FIFO barrier. Duplicate/stale timers return `false`; only the one timer
    /// that claims the matching head should invoke the production drainer.
    pub(crate) async fn wait_and_activate_scheduled_retry(
        &self,
        schedule: RetrySchedule,
    ) -> Result<bool, String> {
        tokio::time::sleep_until(schedule.not_before).await;
        self.activate_scheduled_retry(&schedule)
    }

    fn activate_scheduled_retry(&self, schedule: &RetrySchedule) -> Result<bool, String> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "CRDT operation queue lock poisoned".to_string())?;
        let Some(wait) = inner.retry_waits.get(&schedule.operation_id) else {
            return Ok(false);
        };
        if wait.attempt != schedule.attempt || wait.not_before != schedule.not_before {
            return Ok(false);
        }
        if Instant::now() < wait.not_before {
            return Ok(false);
        }
        let Some(position) = inner
            .pending
            .iter()
            .position(|operation| operation.operation_id == schedule.operation_id)
        else {
            return Ok(false);
        };
        let graph_id = inner.pending[position].graph_id.clone();
        if inner
            .pending
            .iter()
            .take(position)
            .any(|operation| operation.graph_id == graph_id)
        {
            // A retry token may only unbarrier the first pending operation for
            // its graph. This makes stale/corrupt tokens fail closed.
            return Ok(false);
        }
        inner.retry_waits.remove(&schedule.operation_id);
        Ok(true)
    }

    #[cfg(test)]
    pub(crate) fn has_live_responder(&self, operation_id: &str) -> bool {
        let Ok(mut inner) = self.inner.lock() else {
            return false;
        };
        if inner
            .responders
            .get(operation_id)
            .is_some_and(oneshot::Sender::is_closed)
        {
            inner.responders.remove(operation_id);
        }
        inner.responders.contains_key(operation_id)
    }

    pub(super) fn detach_responder(&self, operation_id: &str) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.responders.remove(operation_id);
        }
    }

    #[cfg(test)]
    pub(crate) fn counts_for_test(&self) -> Result<(usize, usize), String> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| "CRDT operation queue lock poisoned".to_string())?;
        Ok((inner.pending.len(), inner.in_flight.len()))
    }

    #[cfg(test)]
    pub(crate) fn retry_wait_count_for_test(&self) -> Result<usize, String> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| "CRDT operation queue lock poisoned".to_string())?;
        Ok(inner.retry_waits.len())
    }

    #[cfg(test)]
    pub(crate) fn drain_available_for_test(&self) -> bool {
        self.drain.try_lock().is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_poll_respects_limit_and_preserves_order() {
        let queue = CrdtOperationQueue::default();
        for id in ["op-1", "op-2"] {
            let operation = CrdtOperation {
                operation_id: id.to_string(),
                kind: "document.write".to_string(),
                graph_id: "graph-1".to_string(),
                document_id: Some("doc-1".to_string()),
                payload: serde_json::json!({}),
                enqueue_timestamp: "0".to_string(),
            };
            let _ = queue.enqueue(operation).expect("enqueue operation");
        }

        let first_batch = queue.poll(Some(1)).expect("poll first operation");
        let second_batch = queue.poll(Some(10)).expect("poll second operation");

        assert_eq!(first_batch.len(), 1);
        assert_eq!(first_batch[0].operation_id, "op-1");
        assert_eq!(second_batch.len(), 1);
        assert_eq!(second_batch[0].operation_id, "op-2");
    }

    #[test]
    fn detached_responder_keeps_pending_operation_recoverable() {
        let queue = CrdtOperationQueue::default();
        let operation = CrdtOperation {
            operation_id: "op-detached".to_string(),
            kind: "document.write".to_string(),
            graph_id: "graph-1".to_string(),
            document_id: Some("doc-1".to_string()),
            payload: serde_json::json!({}),
            enqueue_timestamp: "0".to_string(),
        };
        let _ = queue.enqueue(operation).expect("enqueue operation");

        queue.detach_responder("op-detached");
        let batch = queue.poll(Some(10)).expect("poll detached operation");

        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].operation_id, "op-detached");
    }

    #[test]
    fn detached_recovered_operations_can_be_polled() {
        let queue = CrdtOperationQueue::default();
        queue
            .enqueue_detached(CrdtOperation {
                operation_id: "op-recovered".to_string(),
                kind: "document.write".to_string(),
                graph_id: "graph-1".to_string(),
                document_id: Some("doc-1".to_string()),
                payload: serde_json::json!({}),
                enqueue_timestamp: "0".to_string(),
            })
            .expect("enqueue recovered operation");

        let batch = queue.poll(Some(10)).expect("poll recovered operation");

        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].operation_id, "op-recovered");
    }

    fn retry_operation(id: &str) -> CrdtOperation {
        retry_operation_for_graph(id, "graph-retry")
    }

    fn retry_operation_for_graph(id: &str, graph_id: &str) -> CrdtOperation {
        CrdtOperation {
            operation_id: id.to_string(),
            kind: "workspace.deleteArtifact".to_string(),
            graph_id: graph_id.to_string(),
            document_id: Some("artifact-a".to_string()),
            payload: serde_json::json!({ "artifactId": "artifact-a" }),
            enqueue_timestamp: "0".to_string(),
        }
    }

    fn current_thread_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("current-thread Tokio runtime")
    }

    #[test]
    fn scheduled_retry_is_fifo_barrier_and_later_enqueue_cannot_hammer_before_due() {
        current_thread_runtime().block_on(async {
            tokio::time::pause();
            let queue = std::sync::Arc::new(CrdtOperationQueue::default());
            queue
                .enqueue_detached(retry_operation("op-retry"))
                .expect("enqueue retry head");
            queue
                .enqueue_detached(retry_operation("op-later"))
                .expect("enqueue later operation");
            let first = queue.poll(Some(1)).expect("poll retry head");
            let schedule = queue
                .requeue_retryable_after_hot_commit(first[0].clone())
                .expect("retain original operation");
            assert_eq!(schedule.attempt, 1);
            assert!(!schedule.has_live_responder);
            assert_eq!(
                schedule.delay,
                Duration::from_millis(DETACHED_RETRY_BACKOFF_BASE_MS)
            );
            assert_eq!(queue.retry_wait_count_for_test().unwrap(), 1);

            let timer_queue = queue.clone();
            let timer_schedule = schedule.clone();
            let timer = tokio::spawn(async move {
                timer_queue
                    .wait_and_activate_scheduled_retry(timer_schedule)
                    .await
                    .expect("activate scheduled retry")
            });
            tokio::task::yield_now().await;

            // Repeated early drain attempts cannot repoll op-retry and cannot
            // skip it to reach op-later.
            assert!(queue.poll(Some(1)).unwrap().is_empty());
            assert!(queue.poll(Some(10)).unwrap().is_empty());
            assert_eq!(queue.counts_for_test().unwrap(), (2, 0));
            tokio::time::advance(schedule.delay - Duration::from_millis(1)).await;
            tokio::task::yield_now().await;
            assert!(!timer.is_finished());
            assert!(queue.poll(Some(1)).unwrap().is_empty());

            tokio::time::advance(Duration::from_millis(1)).await;
            assert!(timer.await.expect("retry timer task"));
            let retry = queue.poll(Some(1)).expect("poll due retry");
            assert_eq!(retry.len(), 1);
            assert_eq!(retry[0].operation_id, "op-retry");
            queue
                .complete(CompleteCrdtOperationInput {
                    operation_id: "op-retry".to_string(),
                    ok: true,
                    value: Some(serde_json::json!({ "ok": true })),
                    error: None,
                    timing: None,
                })
                .expect("complete original retry");
            let later = queue.poll(Some(1)).expect("poll later operation");
            assert_eq!(later.len(), 1);
            assert_eq!(later[0].operation_id, "op-later");
        });
    }

    #[test]
    fn waiting_graph_preserves_its_fifo_while_other_graph_drains_in_order() {
        current_thread_runtime().block_on(async {
            tokio::time::pause();
            let queue = std::sync::Arc::new(CrdtOperationQueue::default());
            queue
                .enqueue_detached(retry_operation_for_graph("a1", "graph-a"))
                .unwrap();
            let a1 = queue.poll(Some(1)).unwrap().remove(0);
            let a_schedule = queue
                .requeue_retryable_after_hot_commit(a1)
                .expect("schedule graph A retry");
            for (id, graph) in [("a2", "graph-a"), ("b1", "graph-b"), ("b2", "graph-b")] {
                queue
                    .enqueue_detached(retry_operation_for_graph(id, graph))
                    .unwrap();
            }

            let b1 = queue.poll(Some(1)).expect("poll graph B first op");
            assert_eq!(
                b1.iter()
                    .map(|operation| operation.operation_id.as_str())
                    .collect::<Vec<_>>(),
                vec!["b1"]
            );
            queue
                .complete(CompleteCrdtOperationInput {
                    operation_id: "b1".to_string(),
                    ok: true,
                    value: None,
                    error: None,
                    timing: None,
                })
                .unwrap();
            let b2 = queue.poll(Some(1)).expect("poll graph B second op");
            assert_eq!(
                b2.iter()
                    .map(|operation| operation.operation_id.as_str())
                    .collect::<Vec<_>>(),
                vec!["b2"]
            );
            queue
                .complete(CompleteCrdtOperationInput {
                    operation_id: "b2".to_string(),
                    ok: true,
                    value: None,
                    error: None,
                    timing: None,
                })
                .unwrap();
            assert!(queue.poll(Some(10)).unwrap().is_empty());

            let timer_queue = queue.clone();
            let timer_schedule = a_schedule.clone();
            let timer = tokio::spawn(async move {
                timer_queue
                    .wait_and_activate_scheduled_retry(timer_schedule)
                    .await
                    .unwrap()
            });
            tokio::task::yield_now().await;
            tokio::time::advance(a_schedule.delay).await;
            assert!(timer.await.unwrap());
            let a1 = queue.poll(Some(1)).expect("poll due graph A retry");
            assert_eq!(
                a1.iter()
                    .map(|operation| operation.operation_id.as_str())
                    .collect::<Vec<_>>(),
                vec!["a1"]
            );
            queue
                .complete(CompleteCrdtOperationInput {
                    operation_id: "a1".to_string(),
                    ok: true,
                    value: None,
                    error: None,
                    timing: None,
                })
                .unwrap();
            let a2 = queue.poll(Some(1)).expect("poll graph A successor");
            assert_eq!(a2.len(), 1);
            assert_eq!(a2[0].operation_id, "a2");
        });
    }

    #[test]
    fn multiple_waiting_graphs_do_not_block_a_ready_third_graph() {
        current_thread_runtime().block_on(async {
            tokio::time::pause();
            let queue = CrdtOperationQueue::default();
            queue
                .enqueue_detached(retry_operation_for_graph("a1", "graph-a"))
                .unwrap();
            let a1 = queue.poll(Some(1)).unwrap().remove(0);
            let _a_schedule = queue
                .requeue_retryable_after_hot_commit(a1)
                .expect("schedule graph A");
            queue
                .enqueue_detached(retry_operation_for_graph("a2", "graph-a"))
                .unwrap();
            queue
                .enqueue_detached(retry_operation_for_graph("b1", "graph-b"))
                .unwrap();
            let b1 = queue.poll(Some(1)).unwrap().remove(0);
            assert_eq!(b1.operation_id, "b1");
            let _b_schedule = queue
                .requeue_retryable_after_hot_commit(b1)
                .expect("schedule graph B");
            for id in ["b2", "c1", "c2"] {
                let graph = if id.starts_with('b') {
                    "graph-b"
                } else {
                    "graph-c"
                };
                queue
                    .enqueue_detached(retry_operation_for_graph(id, graph))
                    .unwrap();
            }

            let c1 = queue.poll(Some(1)).expect("poll first graph C op");
            assert_eq!(c1.len(), 1);
            assert_eq!(c1[0].operation_id, "c1");
            queue
                .complete(CompleteCrdtOperationInput {
                    operation_id: "c1".to_string(),
                    ok: true,
                    value: None,
                    error: None,
                    timing: None,
                })
                .unwrap();
            let c2 = queue.poll(Some(1)).expect("poll second graph C op");
            assert_eq!(c2.len(), 1);
            assert_eq!(c2[0].operation_id, "c2");
            assert_eq!(queue.counts_for_test().unwrap(), (4, 1));
        });
    }

    #[test]
    fn detached_recovered_retry_timer_automatically_wakes_without_restart() {
        current_thread_runtime().block_on(async {
            tokio::time::pause();
            let queue = std::sync::Arc::new(CrdtOperationQueue::default());
            queue
                .enqueue_detached(retry_operation("op-recovered-retry"))
                .expect("enqueue recovered operation");
            let operation = queue.poll(Some(1)).unwrap().remove(0);
            let schedule = queue
                .requeue_retryable_after_hot_commit(operation)
                .expect("schedule recovered retry");
            let timer_queue = queue.clone();
            let timer_schedule = schedule.clone();
            let timer = tokio::spawn(async move {
                timer_queue
                    .wait_and_activate_scheduled_retry(timer_schedule)
                    .await
                    .expect("activate recovered retry")
            });
            tokio::task::yield_now().await;
            tokio::time::advance(schedule.delay).await;
            assert!(timer.await.expect("recovered retry timer"));

            let automatically_woken = queue.poll(Some(1)).expect("poll automatic retry");
            assert_eq!(automatically_woken.len(), 1);
            assert_eq!(automatically_woken[0].operation_id, "op-recovered-retry");
        });
    }

    #[test]
    fn dropped_receiver_is_detached_and_does_not_hold_drain_or_hot_loop() {
        current_thread_runtime().block_on(async {
            tokio::time::pause();
            let queue = std::sync::Arc::new(CrdtOperationQueue::default());
            let receiver = queue
                .enqueue(retry_operation("op-cancelled-http"))
                .expect("enqueue attached operation");
            drop(receiver);
            assert!(!queue.has_live_responder("op-cancelled-http"));

            let operation = queue.poll(Some(1)).unwrap().remove(0);
            let first = queue
                .requeue_retryable_after_hot_commit(operation)
                .expect("schedule cancelled caller retry");
            assert!(!first.has_live_responder);
            assert_eq!(
                first.delay,
                Duration::from_millis(DETACHED_RETRY_BACKOFF_BASE_MS)
            );
            assert!(queue.drain_available_for_test());
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            assert_eq!(queue.retry_wait_count_for_test().unwrap(), 1);
            assert!(queue.poll(Some(1)).unwrap().is_empty());

            let timer_queue = queue.clone();
            let timer_schedule = first.clone();
            let timer = tokio::spawn(async move {
                timer_queue
                    .wait_and_activate_scheduled_retry(timer_schedule)
                    .await
                    .unwrap()
            });
            tokio::task::yield_now().await;
            tokio::time::advance(first.delay).await;
            assert!(timer.await.unwrap());
            let retry = queue.poll(Some(1)).unwrap().remove(0);
            let second = queue
                .requeue_retryable_after_hot_commit(retry)
                .expect("schedule second detached attempt");
            assert_eq!(second.attempt, 2);
            assert_eq!(
                second.delay,
                Duration::from_millis(DETACHED_RETRY_BACKOFF_BASE_MS * 2)
            );
            assert!(queue.drain_available_for_test());
        });
    }

    #[test]
    fn duplicate_timer_tokens_claim_once_and_preserve_exact_operation_id() {
        current_thread_runtime().block_on(async {
            tokio::time::pause();
            let queue = std::sync::Arc::new(CrdtOperationQueue::default());
            queue
                .enqueue_detached(retry_operation("op-one-timer"))
                .expect("enqueue operation");
            let operation = queue.poll(Some(1)).unwrap().remove(0);
            let schedule = queue
                .requeue_retryable_after_hot_commit(operation)
                .expect("schedule operation");
            assert_eq!(queue.retry_wait_count_for_test().unwrap(), 1);

            // Even an adversarial duplicate task holding the same token cannot
            // create a second effective wake or duplicate the operation.
            let mut timers = Vec::new();
            for _ in 0..2 {
                let timer_queue = queue.clone();
                let timer_schedule = schedule.clone();
                timers.push(tokio::spawn(async move {
                    timer_queue
                        .wait_and_activate_scheduled_retry(timer_schedule)
                        .await
                        .unwrap()
                }));
            }
            tokio::task::yield_now().await;
            tokio::time::advance(schedule.delay).await;
            let first_claim = timers.remove(0).await.unwrap();
            let second_claim = timers.remove(0).await.unwrap();
            assert_ne!(first_claim, second_claim);
            assert_eq!(queue.retry_wait_count_for_test().unwrap(), 0);

            let retry = queue.poll(Some(10)).expect("poll exact retained operation");
            assert_eq!(retry.len(), 1);
            assert_eq!(retry[0].operation_id, "op-one-timer");
            assert!(queue.poll(Some(1)).unwrap().is_empty());
        });
    }

    #[test]
    fn retry_backoff_caps_for_attached_and_detached_work() {
        current_thread_runtime().block_on(async {
            tokio::time::pause();
            for (id, attached, base_ms, cap_ms) in [
                (
                    "op-attached-cap",
                    true,
                    ATTACHED_RETRY_BACKOFF_BASE_MS,
                    ATTACHED_RETRY_BACKOFF_MAX_MS,
                ),
                (
                    "op-detached-cap",
                    false,
                    DETACHED_RETRY_BACKOFF_BASE_MS,
                    DETACHED_RETRY_BACKOFF_MAX_MS,
                ),
            ] {
                let queue = CrdtOperationQueue::default();
                let _receiver = if attached {
                    Some(
                        queue
                            .enqueue(retry_operation(id))
                            .expect("enqueue attached"),
                    )
                } else {
                    queue
                        .enqueue_detached(retry_operation(id))
                        .expect("enqueue detached");
                    None
                };
                let mut operation = queue.poll(Some(1)).unwrap().remove(0);
                for attempt in 1_u32..=12 {
                    let schedule = queue
                        .requeue_retryable_after_hot_commit(operation)
                        .expect("schedule capped retry");
                    let shift = attempt.saturating_sub(1).min(31);
                    let expected = base_ms
                        .saturating_mul(1_u64.checked_shl(shift).unwrap_or(u64::MAX))
                        .min(cap_ms);
                    assert_eq!(schedule.attempt, attempt);
                    assert_eq!(schedule.delay, Duration::from_millis(expected));
                    tokio::time::advance(schedule.delay).await;
                    assert!(queue
                        .wait_and_activate_scheduled_retry(schedule)
                        .await
                        .unwrap());
                    operation = queue.poll(Some(1)).unwrap().remove(0);
                }
                let capped = queue
                    .requeue_retryable_after_hot_commit(operation)
                    .expect("schedule at cap");
                assert_eq!(capped.delay, Duration::from_millis(cap_ms));
            }
        });
    }

    #[test]
    fn attached_caller_uses_short_backoff_and_explicit_timeout_detaches() {
        current_thread_runtime().block_on(async {
            tokio::time::pause();
            let queue = CrdtOperationQueue::default();
            let _receiver = queue
                .enqueue(retry_operation("op-attached-retry"))
                .expect("enqueue attached retry");
            let operation = queue.poll(Some(1)).unwrap().remove(0);
            let first = queue
                .requeue_retryable_after_hot_commit(operation)
                .expect("schedule attached retry");
            assert!(first.has_live_responder);
            assert_eq!(
                first.delay,
                Duration::from_millis(ATTACHED_RETRY_BACKOFF_BASE_MS)
            );
            tokio::time::advance(first.delay).await;
            assert!(queue
                .wait_and_activate_scheduled_retry(first)
                .await
                .unwrap());
            let retry = queue.poll(Some(1)).unwrap().remove(0);
            queue.detach_responder("op-attached-retry");
            assert!(!queue.has_live_responder("op-attached-retry"));
            let detached = queue
                .requeue_retryable_after_hot_commit(retry)
                .expect("schedule after caller timeout");
            assert!(!detached.has_live_responder);
            assert_eq!(
                detached.delay,
                Duration::from_millis(DETACHED_RETRY_BACKOFF_BASE_MS * 2)
            );
        });
    }

    #[test]
    fn single_flight_drain_cannot_apply_later_operation_first() {
        crate::app_runtime::async_runtime::block_on(async {
            let queue = std::sync::Arc::new(CrdtOperationQueue::default());
            let operation = |id: &str| CrdtOperation {
                operation_id: id.to_string(),
                kind: "workspace.createFolder".to_string(),
                graph_id: "graph-fifo".to_string(),
                document_id: None,
                payload: serde_json::json!({}),
                enqueue_timestamp: "0".to_string(),
            };
            queue
                .enqueue_detached(operation("op-1"))
                .expect("enqueue op1");

            let applied = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
            let (op1_polled_tx, op1_polled_rx) = tokio::sync::oneshot::channel();
            let (release_op1_tx, release_op1_rx) = tokio::sync::oneshot::channel();
            let first_queue = queue.clone();
            let first_applied = applied.clone();
            let first = crate::app_runtime::async_runtime::spawn(async move {
                let _drain = first_queue.lock_drain().await;
                let mut op1_polled_tx = Some(op1_polled_tx);
                let mut release_op1_rx = Some(release_op1_rx);
                loop {
                    let batch = first_queue.poll(Some(1)).expect("first drainer poll");
                    if batch.is_empty() {
                        break;
                    }
                    for operation in batch {
                        if operation.operation_id == "op-1" {
                            let _ = op1_polled_tx.take().expect("op1 polled sender").send(());
                            release_op1_rx
                                .take()
                                .expect("op1 release receiver")
                                .await
                                .expect("release op1");
                        }
                        first_applied
                            .lock()
                            .expect("application order lock")
                            .push(operation.operation_id);
                    }
                }
            });
            op1_polled_rx.await.expect("op1 was polled first");

            queue
                .enqueue_detached(operation("op-2"))
                .expect("enqueue op2 while op1 is applying");
            assert!(
                queue.drain.try_lock().is_err(),
                "later drainer must not enter while op1's drainer is applying"
            );
            let second_queue = queue.clone();
            let second = crate::app_runtime::async_runtime::spawn(async move {
                let _drain = second_queue.lock_drain().await;
                second_queue.poll(Some(8)).expect("second drainer poll")
            });

            release_op1_tx.send(()).expect("unblock op1");
            first.await.expect("first drainer");
            assert!(
                second.await.expect("second drainer").is_empty(),
                "the first drainer must consume op2 before releasing single-flight"
            );
            assert_eq!(
                *applied.lock().expect("final application order"),
                vec!["op-1".to_string(), "op-2".to_string()]
            );
        });
    }
}
