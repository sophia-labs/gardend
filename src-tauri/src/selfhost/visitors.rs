//! `gardend visitors`: the owner's controls for a public Sophia's visitors'
//! folder (W6, `visitor_fence`). Each reads or writes files in
//! `<profile>/public/`; a running gardend sees the change at its next request
//! (the kill switch within a second), so no restart is needed.

use super::cli::VisitorsAction;
use crate::visitor_fence::{AccountsFile, ACCOUNTS_FILE, BANS_FILE, KILL_SWITCH_FILE, VISITOR_STATE_DIR};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

pub(super) fn run(profile_dir: &Path, action: &VisitorsAction) -> Result<String, String> {
    let state_dir = profile_dir.join(VISITOR_STATE_DIR);
    let switch = state_dir.join(KILL_SWITCH_FILE);
    match action {
        VisitorsAction::Off => {
            std::fs::create_dir_all(&state_dir)
                .map_err(|error| format!("create {}: {error}", state_dir.display()))?;
            let stamp = chrono::Utc::now().to_rfc3339();
            crate::storage::write_bytes(&switch, format!("{stamp} turned off: gardend visitors off\n").as_bytes())
                .map_err(|error| error.to_string())?;
            Ok("Visitor writing is off (reading goes on). `gardend visitors on` opens it again.\n".into())
        }
        VisitorsAction::On => {
            match std::fs::remove_file(&switch) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("remove {}: {error}", switch.display())),
            }
            Ok("Visitor writing is on (where public.json opens a visitors' folder).\n".into())
        }
        VisitorsAction::Ban(who) => {
            let banned = crate::visitor_fence::set_banned(&state_dir, who, true)?;
            Ok(format!("Banned {who}. {} account(s) banned.\n", banned.len()))
        }
        VisitorsAction::Unban(who) => {
            let banned = crate::visitor_fence::set_banned(&state_dir, who, false)?;
            Ok(format!("Unbanned {who}. {} account(s) banned.\n", banned.len()))
        }
        VisitorsAction::Status => {
            let folders = configured_folders(profile_dir);
            let mut text = String::new();
            if folders.is_empty() {
                text.push_str("No visitors' folder: public.json has no visitorFolders.\n");
            }
            for (graph, folder) in &folders {
                text.push_str(&format!("Visitors' folder: {graph} / {folder}\n"));
            }
            text.push_str(&format!(
                "Visitor writing: {}\n",
                if switch.exists() { "off" } else { "on" }
            ));
            if let Ok(reason) = std::fs::read_to_string(&switch) {
                text.push_str(&format!("  ({})\n", reason.trim()));
            }
            let accounts = accounts(&state_dir);
            text.push_str(&format!("Accounts seen: {}\n", accounts.accounts.len()));
            text.push_str(&format!("Banned: {}\n", bans(&state_dir).len()));
            Ok(text)
        }
        VisitorsAction::List => {
            let accounts = accounts(&state_dir);
            let banned = bans(&state_dir);
            let mut counts: BTreeMap<String, usize> = BTreeMap::new();
            for (graph, _) in configured_folders(profile_dir) {
                let snapshot = profile_dir
                    .join("graphs")
                    .join(&graph)
                    .join("ydocs/workspace")
                    .join(crate::runtime_config::WORKSPACE_SNAPSHOT_FILE);
                let Ok(value) = crate::storage::read_json::<Value>(&snapshot) else {
                    continue;
                };
                for document in value.get("documents").and_then(Value::as_array).into_iter().flatten() {
                    if let Some(by) = document
                        .get(crate::visitor_fence::CREATED_BY_FIELD)
                        .and_then(Value::as_str)
                    {
                        *counts.entry(by.to_string()).or_default() += 1;
                    }
                }
            }
            if accounts.accounts.is_empty() {
                return Ok("No visitor has written yet.\n".into());
            }
            let mut text = String::from("account            documents  name (first seen)\n");
            for (id, entry) in &accounts.accounts {
                text.push_str(&format!(
                    "{id:<20} {:>8}  {} ({}){}\n",
                    counts.get(id).copied().unwrap_or(0),
                    entry.name,
                    entry.first_seen,
                    if banned.contains(id) { "  BANNED" } else { "" }
                ));
            }
            Ok(text)
        }
    }
}

fn accounts(state_dir: &Path) -> AccountsFile {
    crate::storage::read_json(&state_dir.join(ACCOUNTS_FILE)).unwrap_or_default()
}

fn bans(state_dir: &Path) -> Vec<String> {
    crate::storage::read_json::<Value>(&state_dir.join(BANS_FILE))
        .ok()
        .and_then(|value| value.get("banned").cloned())
        .and_then(|banned| serde_json::from_value(banned).ok())
        .unwrap_or_default()
}

/// The `visitorFolders` of the profile's `public.json` (or
/// `GARDEN_PUBLIC_CONFIG`), as graph and folder.
fn configured_folders(profile_dir: &Path) -> Vec<(String, String)> {
    let path = std::env::var(crate::public_mode::PUBLIC_CONFIG_ENV)
        .ok()
        .filter(|path| !path.trim().is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| profile_dir.join(crate::public_mode::PUBLIC_CONFIG_FILE));
    crate::storage::read_json::<Value>(&path)
        .ok()
        .and_then(|value| value.get("visitorFolders").cloned())
        .and_then(|folders| folders.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|entry| {
            Some((
                entry.get("graphId")?.as_str()?.to_string(),
                entry.get("folderId")?.as_str()?.to_string(),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_on_ban_and_status() {
        let dir = std::env::temp_dir().join(format!("garden-visitors-cli-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("public.json"),
            r#"{"graphs":["g"],"visitorFolders":[{"graphId":"g","folderId":"f"}]}"#,
        )
        .unwrap();
        assert!(run(&dir, &VisitorsAction::Status).unwrap().contains("Visitor writing: on"));
        run(&dir, &VisitorsAction::Off).unwrap();
        assert!(dir.join("public").join(KILL_SWITCH_FILE).exists());
        assert!(run(&dir, &VisitorsAction::Status).unwrap().contains("Visitor writing: off"));
        run(&dir, &VisitorsAction::On).unwrap();
        assert!(!dir.join("public").join(KILL_SWITCH_FILE).exists());
        run(&dir, &VisitorsAction::Ban("acct-0011223344556677".into())).unwrap();
        assert_eq!(bans(&dir.join("public")), vec!["acct-0011223344556677".to_string()]);
        run(&dir, &VisitorsAction::Unban("acct-0011223344556677".into())).unwrap();
        assert!(bans(&dir.join("public")).is_empty());
        assert!(run(&dir, &VisitorsAction::List).unwrap().contains("No visitor"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
