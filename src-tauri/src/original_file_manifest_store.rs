use crate::{
    clock::timestamp,
    ids::safe_filename,
    original_file_types::OriginalFileManifest,
    storage::{display_path, read_json, write_json},
};
use std::{
    fs,
    path::{Component, Path, PathBuf},
};

pub(crate) fn write_original_manifest(
    original_dir: &Path,
    file_path: &Path,
    filename: String,
    mime_type: &str,
    size_bytes: usize,
) -> Result<OriginalFileManifest, String> {
    let manifest_path = original_dir.join("manifest.json");
    let created_at = if manifest_path.is_file() {
        read_json::<OriginalFileManifest>(&manifest_path)
            .map(|manifest| manifest.created_at)
            .unwrap_or_else(|_| timestamp())
    } else {
        timestamp()
    };
    let updated_at = timestamp();
    let manifest = OriginalFileManifest {
        source_filename: None,
        filename,
        mime_type: if mime_type.trim().is_empty() {
            "application/octet-stream".to_string()
        } else {
            mime_type.to_string()
        },
        size_bytes,
        local_path: display_path(file_path),
        created_at,
        updated_at: updated_at.clone(),
    };
    write_json(&manifest_path, &manifest)?;
    Ok(manifest)
}

pub(crate) fn read_original_manifest(original_dir: &Path) -> Result<OriginalFileManifest, String> {
    read_json::<OriginalFileManifest>(&original_dir.join("manifest.json")).map_err(Into::into)
}

pub(crate) fn original_manifest_file_path(
    original_dir: &Path,
    filename: &str,
) -> Result<PathBuf, String> {
    let mut components = Path::new(filename).components();
    let is_single_normal_component =
        matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none();
    if safe_filename(filename) != filename || !is_single_normal_component {
        return Err(format!(
            "original file manifest filename contains unsafe path characters: {filename}"
        ));
    }
    Ok(original_dir.join(filename))
}

pub(crate) fn rewrite_original_manifests_under(root: &Path) -> Result<usize, String> {
    if !root.is_dir() {
        return Ok(0);
    }
    let mut count = 0usize;
    for entry in
        fs::read_dir(root).map_err(|error| format!("read {}: {error}", display_path(root)))?
    {
        let entry = entry.map_err(|error| format!("read original manifest entry: {error}"))?;
        if !entry.path().is_dir() {
            continue;
        }
        let original_dir = entry.path().join("original");
        if original_dir.join("manifest.json").is_file() {
            rewrite_original_manifest_local_path(&original_dir)?;
            count += 1;
        }
    }
    Ok(count)
}

pub(crate) fn rewrite_original_manifest_local_path(original_dir: &Path) -> Result<(), String> {
    let manifest_path = original_dir.join("manifest.json");
    if !manifest_path.is_file() {
        return Ok(());
    }
    let mut manifest = read_original_manifest(original_dir)?;
    manifest.local_path = display_path(&original_manifest_file_path(
        original_dir,
        &manifest.filename,
    )?);
    write_json(&manifest_path, &manifest).map_err(Into::into)
}
