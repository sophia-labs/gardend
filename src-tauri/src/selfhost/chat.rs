//! Settings and secrets for the local services gardend supervises:
//! `gardend service env set|unset|list`, `gardend service enable|disable`
//! and, for chat, `gardend chat key set|clear`.
//!
//! A `service.json` shipped inside a release cannot hold a person's model
//! key, so it lives in the profile, owner-only (`local_service_host`):
//! `<profile>/services.env` reaches every service, `<profile>/services/<id>.env`
//! one service (over it). gardend merges them into a service's environment
//! each time it starts it. `chat key set` is `service env set
//! OPENROUTER_API_KEY` for every service.

use super::serving_now;
use crate::local_service_host::{
    discover_service_ids, gardend_owned_env, is_env_name, is_service_id, read_service_env_file,
    service_env_file, services_dirs_from, shared_service_env_file, write_service_autostart,
    write_service_env_file, CHOREOGRAPH_SERVICE_ID, OPENROUTER_API_KEY_ENV, SERVICES_DIR_ENV,
};
use std::collections::BTreeSet;
use std::io::{BufRead, IsTerminal};
use std::path::{Path, PathBuf};

/// Values are short; anything longer is not one pasted by hand.
const MAX_VALUE_BYTES: usize = 4096;

/// Which settings file: every service's, or one service's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Scope {
    Shared,
    Service(String),
}

impl Scope {
    pub(crate) fn from_flag(service: Option<&str>) -> Result<Self, String> {
        match service {
            None => Ok(Self::Shared),
            Some(id) if is_service_id(id) => Ok(Self::Service(id.to_string())),
            Some(id) => Err(format!(
                "{id:?} is not a service id (lowercase letters, digits and dashes)"
            )),
        }
    }

    fn path(&self, profile_dir: &Path) -> PathBuf {
        match self {
            Self::Shared => shared_service_env_file(profile_dir),
            Self::Service(id) => service_env_file(profile_dir, id),
        }
    }

    fn reaches(&self) -> String {
        match self {
            Self::Shared => "every service gardend supervises".to_string(),
            Self::Service(id) => format!("the {id} service"),
        }
    }
}

/// Read a value from standard input: the first non-empty line. On a
/// terminal, ask for it first (on stderr).
pub(crate) fn read_value_from_stdin(prompt: &str) -> Result<String, String> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        eprint!("{prompt}");
    }
    read_value(stdin.lock())
}

pub(crate) fn read_value(mut input: impl BufRead) -> Result<String, String> {
    let mut line = String::new();
    loop {
        line.clear();
        let read = input
            .read_line(&mut line)
            .map_err(|error| format!("read the value from standard input: {error}"))?;
        if read == 0 {
            return Err(
                "nothing on standard input: paste the value, or pipe it in (`... < file`)"
                    .to_string(),
            );
        }
        if line.len() > MAX_VALUE_BYTES {
            return Err("that line is too long to be a setting".to_string());
        }
        let value = line.trim();
        if !value.is_empty() {
            return Ok(value.to_string());
        }
    }
}

fn check_name(name: &str) -> Result<(), String> {
    if !is_env_name(name) {
        return Err(format!(
            "{name:?} is not a variable name (letters, digits and _, not starting with a digit)"
        ));
    }
    if gardend_owned_env(name) {
        return Err(format!(
            "gardend sets {name} itself for every service; it cannot be overridden"
        ));
    }
    Ok(())
}

pub(crate) fn set_value(
    profile_dir: &Path,
    scope: &Scope,
    name: &str,
    value: &str,
) -> Result<String, String> {
    check_name(name)?;
    let value = value.trim();
    if value.is_empty() {
        return Err(format!(
            "the value of {name} is empty (use `unset` to remove it)"
        ));
    }
    if value.chars().any(char::is_control) {
        return Err("a setting is one line".to_string());
    }
    require_profile(profile_dir)?;
    let path = scope.path(profile_dir);
    let mut entries = read_service_env_file(&path)?;
    entries.retain(|(entry, _)| entry != name);
    entries.push((name.to_string(), value.to_string()));
    write_service_env_file(&path, &scope.reaches(), &entries)?;
    let mut out = format!(
        "Saved {name} for {} in {} (owner-only).\n",
        scope.reaches(),
        path.display()
    );
    out.push_str(&restart_running_services(profile_dir, scope));
    Ok(out)
}

pub(crate) fn unset_value(profile_dir: &Path, scope: &Scope, name: &str) -> Result<String, String> {
    check_name(name)?;
    require_profile(profile_dir)?;
    let path = scope.path(profile_dir);
    let mut entries = read_service_env_file(&path)?;
    let before = entries.len();
    entries.retain(|(entry, _)| entry != name);
    if entries.len() == before {
        return Ok(format!("{name} was not set in {}.\n", path.display()));
    }
    write_service_env_file(&path, &scope.reaches(), &entries)?;
    let mut out = format!("Removed {name} from {}.\n", path.display());
    out.push_str(&restart_running_services(profile_dir, scope));
    Ok(out)
}

/// The names (never the values) in the settings files.
pub(crate) fn list_names(profile_dir: &Path, scope: Option<&Scope>) -> Result<String, String> {
    require_profile(profile_dir)?;
    let mut files = vec![Scope::Shared];
    match scope {
        Some(Scope::Service(id)) => files.push(Scope::Service(id.clone())),
        Some(Scope::Shared) => {}
        None => {
            if let Ok(entries) = std::fs::read_dir(profile_dir.join("services")) {
                let mut ids: Vec<String> = entries
                    .flatten()
                    .filter_map(|entry| {
                        let name = entry.file_name().to_string_lossy().to_string();
                        name.strip_suffix(".env")
                            .filter(|id| is_service_id(id))
                            .map(str::to_string)
                    })
                    .collect();
                ids.sort();
                files.extend(ids.into_iter().map(Scope::Service));
            }
        }
    }
    let mut out = String::new();
    for scope in files {
        let path = scope.path(profile_dir);
        let names: Vec<String> = read_service_env_file(&path)?
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        out.push_str(&format!(
            "{} ({}): {}\n",
            scope.reaches(),
            path.display(),
            if names.is_empty() {
                "nothing set".to_string()
            } else {
                names.join(", ")
            }
        ));
    }
    Ok(out)
}

/// `gardend chat key set`: the OpenRouter key, for every service.
pub(crate) fn set_key(profile_dir: &Path, key: &str) -> Result<String, String> {
    let key = key.trim();
    if key.chars().any(char::is_whitespace) {
        return Err("a key is one word with no spaces; paste just the key".to_string());
    }
    set_value(profile_dir, &Scope::Shared, OPENROUTER_API_KEY_ENV, key)
}

pub(crate) fn clear_key(profile_dir: &Path) -> Result<String, String> {
    unset_value(profile_dir, &Scope::Shared, OPENROUTER_API_KEY_ENV)
}

/// `gardend service enable|disable <id>`: whether gardend starts the service
/// when it boots, for this profile (`<profile>/services/<id>.autostart`, over
/// the service's `autostart`). When a gardend serves the profile, the service
/// starts or stops now, through its API.
pub(crate) fn set_enabled(
    profile_dir: &Path,
    service_id: &str,
    enabled: bool,
) -> Result<String, String> {
    if !is_service_id(service_id) {
        return Err(format!(
            "{service_id:?} is not a service id (lowercase letters, digits and dashes)"
        ));
    }
    require_profile(profile_dir)?;
    let path = write_service_autostart(profile_dir, service_id, enabled)?;
    let mut out = format!(
        "{service_id} is {} for this profile: gardend {} it when it starts ({}).\n",
        if enabled { "enabled" } else { "disabled" },
        if enabled { "starts" } else { "does not start" },
        path.display()
    );
    out.push_str(&apply_enabled_now(profile_dir, service_id, enabled));
    Ok(out)
}

/// Start or stop `service_id` under the gardend serving this profile, if one
/// does, and say what happened.
fn apply_enabled_now(profile_dir: &Path, service_id: &str, enabled: bool) -> String {
    let Some(running) = serving_now(profile_dir) else {
        return "No gardend serves this profile now; it takes effect when `gardend serve` starts.\n"
            .to_string();
    };
    let action = if enabled { "start" } else { "stop" };
    let base = format!("http://127.0.0.1:{}", running.port);
    let token = running.token.clone();
    let path = format!("/api/services/{service_id}/{action}");
    let outcome = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())
        .and_then(|runtime| {
            runtime.block_on(async move {
                let response = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(60))
                    .build()
                    .map_err(|error| error.to_string())?
                    .post(format!("{base}{path}"))
                    .bearer_auth(&token)
                    .send()
                    .await
                    .map_err(|error| error.to_string())?;
                let status = response.status();
                let body = response
                    .json::<serde_json::Value>()
                    .await
                    .unwrap_or(serde_json::Value::Null);
                Ok::<_, String>((status, body))
            })
        });
    let describe = |body: &serde_json::Value| {
        body.get("reason")
            .or_else(|| body.get("error"))
            .or_else(|| body.get("lastError"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    match outcome {
        Ok((status, body)) if status.is_success() => {
            let state = body
                .get("state")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            match describe(&body) {
                Some(reason) if state == "unavailable" => format!(
                    "The gardend serving this profile (pid {}) could not start it: {reason}\n",
                    running.pid
                ),
                _ => format!(
                    "{} under the gardend serving this profile (pid {}): {state}.\n",
                    if enabled { "Started" } else { "Stopped" },
                    running.pid
                ),
            }
        }
        Ok((status, body)) => format!(
            "The gardend serving this profile (pid {}) answered {status} to {action}: {}\n",
            running.pid,
            describe(&body).unwrap_or_else(|| body.to_string())
        ),
        Err(error) => format!(
            "Could not reach the gardend serving this profile ({error}); it takes effect when gardend restarts.\n"
        ),
    }
}

fn require_profile(profile_dir: &Path) -> Result<(), String> {
    if profile_dir.is_dir() {
        Ok(())
    } else {
        Err(format!(
            "no profile at {}: start it once with `gardend serve`, or name it with --profile",
            profile_dir.display()
        ))
    }
}

/// The services a running gardend might be running, as this shell sees them:
/// Choreograph, any `GARDEN_<ID>_MANIFEST`, `GARDEN_SERVICES_DIR` and the
/// `services/` folder beside this binary.
fn candidate_services(scope: &Scope) -> BTreeSet<String> {
    if let Scope::Service(id) = scope {
        return BTreeSet::from([id.clone()]);
    }
    let resource_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| crate::app_runtime::binary_resource_dir(&exe));
    let dirs = services_dirs_from(
        std::env::var(SERVICES_DIR_ENV).ok().as_deref(),
        resource_dir.as_deref(),
    );
    let mut ids = discover_service_ids(std::env::vars(), &dirs);
    ids.insert(CHOREOGRAPH_SERVICE_ID.to_string());
    ids
}

/// When a gardend serves this profile, restart the services it runs that
/// read this file, through its API, so the change takes effect now; else say
/// when it will.
fn restart_running_services(profile_dir: &Path, scope: &Scope) -> String {
    const LATER: &str = "It takes effect the next time gardend starts the service.\n";
    let Some(running) = serving_now(profile_dir) else {
        return LATER.to_string();
    };
    let base = format!("http://127.0.0.1:{}", running.port);
    let token = running.token.clone();
    let candidates = candidate_services(scope);
    let outcome = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())
        .and_then(|runtime| {
            runtime.block_on(async move {
                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(30))
                    .build()
                    .map_err(|error| error.to_string())?;
                let call = |method: reqwest::Method, path: String| {
                    client
                        .request(method, format!("{base}{path}"))
                        .bearer_auth(&token)
                        .send()
                };
                let mut restarted = Vec::new();
                for id in candidates {
                    let Ok(response) =
                        call(reqwest::Method::GET, format!("/api/services/{id}/status")).await
                    else {
                        continue;
                    };
                    let Ok(status) = response.json::<serde_json::Value>().await else {
                        continue;
                    };
                    if status.get("state").and_then(serde_json::Value::as_str) != Some("running") {
                        continue;
                    }
                    for action in ["stop", "start"] {
                        let response = call(
                            reqwest::Method::POST,
                            format!("/api/services/{id}/{action}"),
                        )
                        .await
                        .map_err(|error| error.to_string())?;
                        if !response.status().is_success() {
                            return Err(format!("POST {action} {id}: {}", response.status()));
                        }
                    }
                    restarted.push(id);
                }
                Ok(restarted)
            })
        });
    match outcome {
        Ok(restarted) if restarted.is_empty() => LATER.to_string(),
        Ok(restarted) => format!(
            "Restarted {} under the gardend serving this profile (pid {}) to use it.\n",
            restarted.join(", "),
            running.pid
        ),
        Err(error) => format!(
            "Could not restart the services under the running gardend ({error}); restart gardend to use it.\n"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::tempdir;
    use super::*;

    #[test]
    fn a_value_is_read_from_the_first_non_empty_line() {
        assert_eq!(
            read_value(std::io::Cursor::new("\n  sk-or-v1-abc  \nignored\n")).unwrap(),
            "sk-or-v1-abc"
        );
        assert!(read_value(std::io::Cursor::new("\n\n")).is_err());
        assert!(read_value(std::io::Cursor::new("x".repeat(MAX_VALUE_BYTES + 1))).is_err());
    }

    #[test]
    fn settings_are_kept_per_scope_owner_only_and_never_listed_with_values() {
        let profile = tempdir("service-env").join("profile");
        assert!(set_key(&profile, "sk-or-v1-abc")
            .unwrap_err()
            .contains("no profile"));
        std::fs::create_dir_all(&profile).unwrap();
        let shared = shared_service_env_file(&profile);
        std::fs::write(
            &shared,
            "CHOREOGRAPH_WORKSPACE_CHAT_MODEL=google/gemini-2.5-flash\nOPENROUTER_API_KEY=old\n",
        )
        .unwrap();

        let said = set_key(&profile, " sk-or-v1-new ").unwrap();
        assert!(
            said.contains("Saved OPENROUTER_API_KEY for every service"),
            "{said}"
        );
        assert!(said.contains("next time gardend starts"), "{said}");
        assert_eq!(
            read_service_env_file(&shared).unwrap(),
            vec![
                (
                    "CHOREOGRAPH_WORKSPACE_CHAT_MODEL".to_string(),
                    "google/gemini-2.5-flash".to_string()
                ),
                ("OPENROUTER_API_KEY".to_string(), "sk-or-v1-new".to_string()),
            ]
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&shared).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(set_key(&profile, "two words").is_err());
        assert!(set_key(&profile, "").is_err());

        // One service's own settings.
        let vehicle = Scope::from_flag(Some("vehicle")).unwrap();
        set_value(&profile, &vehicle, "ANTHROPIC_API_KEY", "sk-ant-x").unwrap();
        assert_eq!(
            read_service_env_file(&service_env_file(&profile, "vehicle")).unwrap(),
            vec![("ANTHROPIC_API_KEY".to_string(), "sk-ant-x".to_string())]
        );
        let listed = list_names(&profile, None).unwrap();
        assert!(listed.contains("OPENROUTER_API_KEY") && listed.contains("ANTHROPIC_API_KEY"));
        assert!(
            !listed.contains("sk-or-v1-new") && !listed.contains("sk-ant-x"),
            "values are never listed: {listed}"
        );
        for refused in [
            "PORT",
            "GARDEN_LOOPBACK_TOKEN",
            "MNEMOSYNE_MCP_ACCESS_TOKEN",
            "9X",
            "A-B",
        ] {
            assert!(
                set_value(&profile, &Scope::Shared, refused, "x").is_err(),
                "{refused}"
            );
        }
        assert!(Scope::from_flag(Some("../etc")).is_err());
        assert!(set_value(&profile, &Scope::Shared, "X", "a\nb").is_err());

        assert!(clear_key(&profile).unwrap().contains("Removed"));
        assert!(clear_key(&profile).unwrap().contains("was not set"));
        assert!(unset_value(&profile, &vehicle, "ANTHROPIC_API_KEY")
            .unwrap()
            .contains("Removed"));
    }

    #[test]
    fn enable_and_disable_record_the_profile_choice_owner_only() {
        let profile = tempdir("service-enable").join("profile");
        assert!(set_enabled(&profile, "choreograph", true)
            .unwrap_err()
            .contains("no profile"));
        std::fs::create_dir_all(&profile).unwrap();
        let said = set_enabled(&profile, "choreograph", true).unwrap();
        assert!(said.contains("choreograph is enabled"), "{said}");
        assert!(said.contains("when `gardend serve` starts"), "{said}");
        let path = crate::local_service_host::service_autostart_file(&profile, "choreograph");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "on\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(set_enabled(&profile, "vehicle", false)
            .unwrap()
            .contains("vehicle is disabled"));
        assert_eq!(
            crate::local_service_host::read_service_autostart(&profile, "vehicle").unwrap(),
            Some(false)
        );
        assert!(set_enabled(&profile, "Not/An-Id", true).is_err());
    }
}
