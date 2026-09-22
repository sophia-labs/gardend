#[cfg(not(test))]
use crate::cell_lifecycle::process_lifecycle;
use crate::{
    app_error::{AppError, AppResult},
    cell_lifecycle::CellLifecycle,
    clock::epoch_millis,
    ids::validate_local_id,
    local_job_record_builder::{finished_job_record, new_local_job_id, queued_job_record},
    local_job_results::{apply_job_result_to_detail, read_spilled_job_result},
    local_job_store::{
        ensure_job_dir, load_job_records, persist_job_record, read_job_record, result_path,
    },
    local_job_transitions::{
        apply_record_progress, cancel_record, finish_record, mark_record_running,
    },
    local_job_types::{LocalJobCancelResponse, LocalJobProgress, LocalJobRecord, LocalJobStatus},
};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

pub(crate) struct LocalJobRegistry {
    jobs_dir: PathBuf,
    records: Mutex<BTreeMap<String, LocalJobRecord>>,
    lifecycle: Option<Arc<CellLifecycle>>,
    cell_authority: Option<CellJobAuthority>,
}

#[derive(Debug, Clone)]
struct CellJobAuthority {
    owner: String,
    graph_id: String,
    generation: u64,
}

impl LocalJobRegistry {
    pub(crate) fn new(jobs_dir: PathBuf) -> AppResult<Self> {
        // Unit tests run many MockRuntime handles concurrently in one process;
        // binding an ordinary registry to the process-global weak slot would
        // couple unrelated tests. Lifecycle tests inject their tracker through
        // `new_with_lifecycle` explicitly.
        #[cfg(test)]
        let lifecycle = None;
        #[cfg(not(test))]
        let lifecycle = process_lifecycle();
        Self::new_with_authority(jobs_dir, lifecycle, None)
    }

    pub(crate) fn new_bound(
        jobs_dir: PathBuf,
        boundary: &crate::cell_graph_boundary::CellGraphBoundary,
    ) -> AppResult<Self> {
        #[cfg(test)]
        let lifecycle = None;
        #[cfg(not(test))]
        let lifecycle = process_lifecycle();
        let authority = match (
            boundary.owner_principal(),
            boundary.owner_graph_id(),
            boundary.graph_generation(),
        ) {
            (Some(owner), Some(graph_id), Some(generation)) => Some(CellJobAuthority {
                owner: owner.to_string(),
                graph_id: graph_id.to_string(),
                generation,
            }),
            _ => None,
        };
        Self::new_with_authority(jobs_dir, lifecycle, authority)
    }

    pub(crate) fn new_with_lifecycle(
        jobs_dir: PathBuf,
        lifecycle: Option<Arc<CellLifecycle>>,
    ) -> AppResult<Self> {
        Self::new_with_authority(jobs_dir, lifecycle, None)
    }

    fn new_with_authority(
        jobs_dir: PathBuf,
        lifecycle: Option<Arc<CellLifecycle>>,
        cell_authority: Option<CellJobAuthority>,
    ) -> AppResult<Self> {
        let records = load_job_records(&jobs_dir).map_err(AppError::database)?;

        Ok(Self {
            jobs_dir,
            records: Mutex::new(records),
            lifecycle,
            cell_authority,
        })
    }

    fn bind_submission(
        &self,
        record: &mut LocalJobRecord,
        graph_id: Option<&str>,
    ) -> AppResult<()> {
        let Some(authority) = &self.cell_authority else {
            return Ok(());
        };
        let Some(graph_id) = graph_id else {
            return Ok(());
        };
        if graph_id != authority.graph_id {
            return Err(AppError::not_found("graph not found in this cell"));
        }
        let lease = crate::cell_graph_boundary::current_cell_lease()
            .ok_or_else(|| AppError::not_found("graph job authorization is unavailable"))?;
        record.owner_principal = Some(authority.owner.clone());
        record.graph_generation = Some(authority.generation);
        record.initiator_principal = Some(lease.principal);
        record.submitted_role = Some(
            match lease.role {
                crate::cell_graph_boundary::CellRole::Viewer => "viewer",
                crate::cell_graph_boundary::CellRole::Editor => "editor",
                crate::cell_graph_boundary::CellRole::Owner => "owner",
            }
            .to_string(),
        );
        record.policy_revision = Some(lease.policy_revision);
        Ok(())
    }

    pub(crate) fn insert_queued(
        &self,
        job_type: &str,
        graph_id: Option<String>,
        detail: serde_json::Value,
    ) -> AppResult<LocalJobRecord> {
        let job_id = new_local_job_id();
        let now = epoch_millis().to_string();
        let mut record =
            queued_job_record(job_id.clone(), job_type, graph_id.as_deref(), detail, now)
                .map_err(AppError::serialization)?;
        self.bind_submission(&mut record, graph_id.as_deref())?;
        if let Some(lifecycle) = &self.lifecycle {
            lifecycle
                .job_started(&job_id)
                .map_err(|error| AppError::internal(error.to_string()))?;
        }
        let inserted = (|| {
            let mut records = self
                .records
                .lock()
                .map_err(|_| AppError::internal("local job registry lock poisoned"))?;
            records.insert(job_id.clone(), record.clone());
            drop(records);
            persist_job_record(&self.jobs_dir, &record).map_err(AppError::database)?;
            Ok::<(), AppError>(())
        })();
        if let Err(error) = inserted {
            if let Some(lifecycle) = &self.lifecycle {
                lifecycle.job_finished(&job_id);
            }
            return Err(error);
        }
        Ok(record)
    }

    pub(crate) fn mark_running(&self, job_id: &str) -> AppResult<Option<LocalJobRecord>> {
        let Some(mut record) = self.get(job_id)? else {
            return Ok(None);
        };
        if !mark_record_running(&mut record) {
            return Ok(Some(record));
        }
        persist_job_record(&self.jobs_dir, &record).map_err(AppError::database)?;
        let mut records = self
            .records
            .lock()
            .map_err(|_| AppError::internal("local job registry lock poisoned"))?;
        records.insert(job_id.to_string(), record.clone());
        Ok(Some(record))
    }

    pub(crate) fn finish_existing(
        &self,
        job_id: &str,
        result: Result<serde_json::Value, String>,
        result_mime_type: &str,
    ) -> AppResult<Option<LocalJobRecord>> {
        let Some(mut record) = self.get(job_id)? else {
            return Ok(None);
        };
        let result_path = result_path(&self.jobs_dir, job_id).map_err(AppError::validation)?;
        if !finish_record(&mut record, &result_path, result, result_mime_type)
            .map_err(AppError::storage)?
        {
            return Ok(Some(record));
        }
        persist_job_record(&self.jobs_dir, &record).map_err(AppError::database)?;
        let mut records = self
            .records
            .lock()
            .map_err(|_| AppError::internal("local job registry lock poisoned"))?;
        records.insert(job_id.to_string(), record.clone());
        if let Some(lifecycle) = &self.lifecycle {
            lifecycle.job_finished(job_id);
        }
        Ok(Some(record))
    }

    pub(crate) fn update_progress(
        &self,
        job_id: &str,
        progress: LocalJobProgress,
    ) -> AppResult<Option<LocalJobRecord>> {
        let Some(mut record) = self.get(job_id)? else {
            return Ok(None);
        };
        if !apply_record_progress(&mut record, progress).map_err(AppError::serialization)? {
            return Ok(Some(record));
        }
        persist_job_record(&self.jobs_dir, &record).map_err(AppError::database)?;
        let mut records = self
            .records
            .lock()
            .map_err(|_| AppError::internal("local job registry lock poisoned"))?;
        records.insert(job_id.to_string(), record.clone());
        Ok(Some(record))
    }

    pub(crate) fn insert_finished(
        &self,
        job_type: &str,
        graph_id: Option<String>,
        started_at_ms: u128,
        result: Result<serde_json::Value, String>,
        result_mime_type: &str,
    ) -> AppResult<LocalJobRecord> {
        let job_id = new_local_job_id();
        let completed_at_ms = epoch_millis();
        ensure_job_dir(&self.jobs_dir, &job_id).map_err(AppError::storage)?;
        let result_path = result_path(&self.jobs_dir, &job_id).map_err(AppError::validation)?;

        let mut detail = serde_json::json!({ "type": job_type });
        let materialized = apply_job_result_to_detail(
            &mut detail,
            &job_id,
            &result_path,
            result,
            result_mime_type,
        )
        .map_err(AppError::storage)?;
        let mut record = finished_job_record(
            job_id.clone(),
            graph_id.as_deref(),
            started_at_ms,
            completed_at_ms,
            detail,
            materialized.status,
            materialized.error,
        );
        self.bind_submission(&mut record, graph_id.as_deref())?;

        let mut records = self
            .records
            .lock()
            .map_err(|_| AppError::internal("local job registry lock poisoned"))?;
        records.insert(job_id, record.clone());
        drop(records);

        persist_job_record(&self.jobs_dir, &record).map_err(AppError::database)?;
        Ok(record)
    }

    pub(crate) fn get(&self, job_id: &str) -> AppResult<Option<LocalJobRecord>> {
        validate_local_id(job_id, "job_id").map_err(AppError::validation)?;
        {
            let records = self
                .records
                .lock()
                .map_err(|_| AppError::internal("local job registry lock poisoned"))?;
            if let Some(record) = records.get(job_id) {
                return Ok(Some(record.clone()));
            }
        }

        let Some(record) = read_job_record(&self.jobs_dir, job_id).map_err(AppError::database)?
        else {
            return Ok(None);
        };
        let mut records = self
            .records
            .lock()
            .map_err(|_| AppError::internal("local job registry lock poisoned"))?;
        records.insert(job_id.to_string(), record.clone());
        Ok(Some(record))
    }

    pub(crate) fn get_fresh(&self, job_id: &str) -> AppResult<Option<LocalJobRecord>> {
        validate_local_id(job_id, "job_id").map_err(AppError::validation)?;
        let Some(record) = read_job_record(&self.jobs_dir, job_id).map_err(AppError::database)?
        else {
            return Ok(None);
        };
        let mut records = self
            .records
            .lock()
            .map_err(|_| AppError::internal("local job registry lock poisoned"))?;
        records.insert(job_id.to_string(), record.clone());
        Ok(Some(record))
    }

    pub(crate) fn is_cancelled(&self, job_id: &str) -> AppResult<bool> {
        Ok(self
            .get_fresh(job_id)?
            .map(|record| matches!(record.status, LocalJobStatus::Cancelled))
            .unwrap_or(false))
    }

    pub(crate) fn cancel(&self, job_id: &str) -> AppResult<Option<LocalJobCancelResponse>> {
        let Some(mut record) = self.get_fresh(job_id)?.or(self.get(job_id)?) else {
            return Ok(None);
        };
        let response = cancel_record(&mut record).map_err(AppError::serialization)?;
        if response.cancelled {
            persist_job_record(&self.jobs_dir, &record).map_err(AppError::database)?;
            let mut records = self
                .records
                .lock()
                .map_err(|_| AppError::internal("local job registry lock poisoned"))?;
            records.insert(job_id.to_string(), record.clone());
            if let Some(lifecycle) = &self.lifecycle {
                lifecycle.job_finished(job_id);
            }
        }

        Ok(Some(response))
    }

    pub(crate) fn read_spilled_result(
        &self,
        record: &LocalJobRecord,
    ) -> AppResult<Option<serde_json::Value>> {
        let path = result_path(&self.jobs_dir, &record.job_id).map_err(AppError::validation)?;
        read_spilled_job_result(record, &path).map_err(AppError::storage)
    }
}
