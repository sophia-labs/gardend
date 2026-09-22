use crate::{
    storage::{copy_file_atomic, create_dir_all, display_path, remove_file_if_exists},
    storage_atomic::sync_parent_dir,
};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};
use uuid::Uuid;

fn pending_uploads_dir(graph_dir: &Path) -> PathBuf {
    graph_dir.join("pending-uploads")
}

pub(crate) fn copy_pending_upload_file(graph_dir: &Path, source: &Path) -> Result<PathBuf, String> {
    let pending_dir = pending_uploads_dir(graph_dir);
    create_dir_all(&pending_dir)?;
    let pending_path = pending_dir.join(format!("upload-{}.bin", Uuid::new_v4().simple()));
    copy_file_atomic(source, &pending_path).map_err(|error| {
        format!(
            "copy pending upload file {}: {error}",
            display_path(&pending_path)
        )
    })?;
    Ok(pending_path)
}

pub(crate) struct PendingUploadFileWriter {
    path: PathBuf,
    file: File,
    bytes_written: usize,
    finished: bool,
}

#[derive(Debug)]
pub(crate) enum PendingUploadWriteError {
    TooLarge { max_bytes: usize },
    Write(String),
}

impl PendingUploadWriteError {
    pub(crate) fn message(&self) -> String {
        match self {
            Self::TooLarge { max_bytes } => {
                format!(
                    "File too large. Maximum size: {}MB",
                    max_bytes / (1024 * 1024)
                )
            }
            Self::Write(message) => message.clone(),
        }
    }
}

pub(crate) fn create_pending_upload_file_writer(
    graph_dir: &Path,
) -> Result<PendingUploadFileWriter, String> {
    let pending_dir = pending_uploads_dir(graph_dir);
    create_dir_all(&pending_dir)?;
    let pending_path = pending_dir.join(format!("upload-{}.bin", Uuid::new_v4().simple()));
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&pending_path)
        .map_err(|error| {
            format!(
                "create pending upload file {}: {error}",
                display_path(&pending_path)
            )
        })?;
    Ok(PendingUploadFileWriter {
        path: pending_path,
        file,
        bytes_written: 0,
        finished: false,
    })
}

impl PendingUploadFileWriter {
    pub(crate) fn write_chunk(
        &mut self,
        chunk: &[u8],
        max_bytes: usize,
    ) -> Result<(), PendingUploadWriteError> {
        let next_len = self
            .bytes_written
            .checked_add(chunk.len())
            .ok_or(PendingUploadWriteError::TooLarge { max_bytes })?;
        if next_len > max_bytes {
            return Err(PendingUploadWriteError::TooLarge { max_bytes });
        }
        self.file.write_all(chunk).map_err(|error| {
            PendingUploadWriteError::Write(format!(
                "write pending upload file {}: {error}",
                display_path(&self.path)
            ))
        })?;
        self.bytes_written = next_len;
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<(PathBuf, usize), String> {
        self.file.sync_all().map_err(|error| {
            format!(
                "sync pending upload file {}: {error}",
                display_path(&self.path)
            )
        })?;
        if let Some(parent) = self.path.parent() {
            sync_parent_dir(parent)?;
        }
        self.finished = true;
        Ok((self.path.clone(), self.bytes_written))
    }
}

impl Drop for PendingUploadFileWriter {
    fn drop(&mut self) {
        if !self.finished {
            cleanup_pending_upload_file(&self.path);
        }
    }
}

pub(crate) fn validate_pending_upload_path(
    graph_dir: &Path,
    pending_path: &str,
) -> Result<PathBuf, String> {
    let pending_dir = pending_uploads_dir(graph_dir);
    let canonical_dir = fs::canonicalize(&pending_dir).map_err(|error| {
        format!(
            "resolve pending uploads directory {}: {error}",
            display_path(&pending_dir)
        )
    })?;
    let path = PathBuf::from(pending_path);
    let canonical_path = fs::canonicalize(&path)
        .map_err(|error| format!("resolve pending upload path {pending_path}: {error}"))?;
    if !canonical_path.starts_with(&canonical_dir) {
        return Err(
            "pending upload path is outside the graph pending upload directory".to_string(),
        );
    }
    Ok(canonical_path)
}

pub(crate) fn cleanup_pending_upload_file(path: &Path) {
    if let Err(error) = remove_file_if_exists(path) {
        log::debug!(
            "failed to remove pending upload {}: {error}",
            display_path(path)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_graph_dir(name: &str) -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("mnemosyne-pending-{name}-{suffix}"))
    }

    #[test]
    fn pending_upload_validation_rejects_paths_outside_graph_pending_dir() {
        let graph_dir = temp_graph_dir("pending");
        fs::create_dir_all(&graph_dir).expect("create graph dir");
        let mut writer =
            create_pending_upload_file_writer(&graph_dir).expect("create pending writer");
        writer.write_chunk(b"abc", 3).expect("write pending upload");
        let (pending_path, _) = writer.finish().expect("finish pending upload");
        let valid = validate_pending_upload_path(&graph_dir, &display_path(&pending_path))
            .expect("validate pending path");
        assert_eq!(
            valid,
            fs::canonicalize(&pending_path).expect("canonical pending")
        );

        let outside_path = graph_dir
            .parent()
            .expect("temp graph has parent")
            .join("outside-upload.bin");
        fs::write(&outside_path, b"abc").expect("write outside file");
        let error = validate_pending_upload_path(&graph_dir, &display_path(&outside_path))
            .expect_err("outside path must be rejected");
        assert!(error.contains("outside the graph pending upload directory"));
        let _ = fs::remove_file(outside_path);
        let _ = fs::remove_dir_all(graph_dir);
    }

    #[test]
    fn pending_upload_writer_streams_and_enforces_size_limit() {
        let graph_dir = temp_graph_dir("writer");
        fs::create_dir_all(&graph_dir).expect("create graph dir");
        let mut writer =
            create_pending_upload_file_writer(&graph_dir).expect("create pending writer");

        writer.write_chunk(b"ab", 3).expect("write first chunk");
        let error = writer
            .write_chunk(b"cd", 3)
            .expect_err("second chunk exceeds limit");
        assert!(matches!(error, PendingUploadWriteError::TooLarge { .. }));
        let path = writer.path.clone();
        drop(writer);
        assert!(!path.exists(), "unfinished writer should clean up");

        let mut writer =
            create_pending_upload_file_writer(&graph_dir).expect("create second pending writer");
        writer.write_chunk(b"abc", 3).expect("write within limit");
        let (path, bytes_written) = writer.finish().expect("finish pending upload");
        assert_eq!(bytes_written, 3);
        assert_eq!(fs::read(&path).expect("read pending bytes"), b"abc");
        let _ = fs::remove_dir_all(graph_dir);
    }

    #[test]
    fn copy_pending_upload_file_copies_without_inline_bytes() {
        let graph_dir = temp_graph_dir("copy");
        fs::create_dir_all(&graph_dir).expect("create graph dir");
        let source = graph_dir.join("source.bin");
        fs::write(&source, b"abc").expect("write source");

        let pending_path = copy_pending_upload_file(&graph_dir, &source).expect("copy pending");

        assert_eq!(fs::read(&pending_path).expect("read pending"), b"abc");
        assert_ne!(pending_path, source);
        let _ = fs::remove_dir_all(graph_dir);
    }
}
