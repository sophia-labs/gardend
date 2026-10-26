use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

use crate::app_error::{AppError, AppResult};
pub use crate::storage_atomic::display_path;
use crate::storage_file_ops::write_secret_bytes;
pub use crate::storage_file_ops::{
    append_secret_line, copy_file_atomic, create_dir_all, read_bytes, remove_dir_all,
    remove_file_if_exists, write_bytes, write_bytes_if_changed,
};

pub fn read_json<T>(path: &Path) -> AppResult<T>
where
    T: for<'de> Deserialize<'de>,
{
    let data = fs::read_to_string(path).map_err(|error| {
        AppError::from(crate::app_error::PersistenceError::io(
            error,
            format!("read {}", display_path(path)),
        ))
    })?;
    serde_json::from_str(&data)
        .map_err(|error| AppError::serialization(format!("parse {}: {error}", display_path(path))))
}

pub fn write_json<T>(path: &Path, value: &T) -> AppResult<()>
where
    T: Serialize,
{
    let data = serde_json::to_string_pretty(value)
        .map_err(|error| AppError::serialization(format!("serialize json: {error}")))?;
    write_bytes(path, format!("{data}\n").as_bytes())
}

/// Compact JSON (one line and a newline), written only when the file's bytes would
/// change. For derived projection files that are only ever parsed, never read by
/// eye or diffed. Returns whether the file was written.
pub fn write_json_compact_if_changed<T>(path: &Path, value: &T) -> AppResult<bool>
where
    T: Serialize,
{
    let mut data = serde_json::to_vec(value)
        .map_err(|error| AppError::serialization(format!("serialize json: {error}")))?;
    data.push(b'\n');
    write_bytes_if_changed(path, &data)
}

pub fn write_secret_json<T>(path: &Path, value: &T) -> AppResult<()>
where
    T: Serialize,
{
    let data = serde_json::to_string_pretty(value)
        .map_err(|error| AppError::serialization(format!("serialize json: {error}")))?;
    write_secret_bytes(path, format!("{data}\n").as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_test_path(name: &str) -> std::path::PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("mnemosyne-storage-{name}-{suffix}.json"))
    }

    #[test]
    fn write_and_read_json_round_trips_pretty_json() {
        let path = temp_test_path("roundtrip");
        let value = json!({ "alpha": 1, "beta": ["two"] });
        write_json(&path, &value).expect("write json");
        let raw = fs::read_to_string(&path).expect("read raw json");
        assert!(raw.ends_with('\n'));
        let parsed: serde_json::Value = read_json(&path).expect("read json");
        assert_eq!(parsed, value);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn compact_if_changed_skips_identical_bytes_and_rewrites_real_changes() {
        let path = temp_test_path("compact");
        let value = json!({ "alpha": 1, "beta": ["two"] });
        assert!(write_json_compact_if_changed(&path, &value).expect("first write"));
        let raw = fs::read_to_string(&path).expect("read raw json");
        assert_eq!(raw, "{\"alpha\":1,\"beta\":[\"two\"]}\n", "compact, one line");
        assert_eq!(read_json::<serde_json::Value>(&path).expect("parse"), value);

        #[cfg(unix)]
        let inode = |path: &Path| {
            use std::os::unix::fs::MetadataExt;
            fs::metadata(path).expect("metadata").ino()
        };
        #[cfg(unix)]
        let before = inode(&path);
        assert!(
            !write_json_compact_if_changed(&path, &value).expect("identical write"),
            "identical bytes are not written"
        );
        // An atomic rewrite renames a new file into place, so the inode would change.
        #[cfg(unix)]
        assert_eq!(inode(&path), before, "the existing file was left alone");

        // The file edited from outside is put back (same length, other content).
        fs::write(&path, "{\"alpha\":2,\"beta\":[\"two\"]}\n").expect("edit from outside");
        assert!(write_json_compact_if_changed(&path, &value).expect("repair write"));
        assert_eq!(read_json::<serde_json::Value>(&path).expect("parse"), value);

        let changed = json!({ "alpha": 1, "beta": ["three"] });
        assert!(write_json_compact_if_changed(&path, &changed).expect("changed write"));
        assert_eq!(
            read_json::<serde_json::Value>(&path).expect("parse"),
            changed
        );
        // A pretty file with the same value is a different file: rewritten once.
        write_json(&path, &changed).expect("pretty write");
        assert!(write_json_compact_if_changed(&path, &changed).expect("compact over pretty"));
        assert!(!write_json_compact_if_changed(&path, &changed).expect("then stable"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn write_secret_json_creates_parent_directory() {
        let root = temp_test_path("secret").with_extension("");
        let path = root.join("nested").join("secret.json");
        let value = json!({ "token": "opaque" });
        write_secret_json(&path, &value).expect("write secret json");
        write_secret_json(&path, &json!({ "token": "replacement" }))
            .expect("overwrite secret json");
        let parsed: serde_json::Value = read_json(&path).expect("read secret json");
        assert_eq!(parsed, json!({ "token": "replacement" }));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path)
                .expect("secret metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = fs::remove_dir_all(root);
    }
}
