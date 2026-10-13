//! `gardend serve`: everything decided before the engine boots, and the
//! banner printed once it is ready.

use super::{
    default_profile_dir, default_ui_dir, ensure_profile_dir, ensure_profile_key, link_host,
    open_url, process_env, read_running_manifest, resolve_host, resolve_port,
    resolve_profile_dir, serving_now, Env, KeySource, ServeArgs, ENV_PORT, ENV_PROFILE_DIR,
    ENV_TOKEN, ENV_UI_DIR, KEY_FILE,
};
use std::net::IpAddr;
use std::path::{Path, PathBuf};

/// What `gardend serve` should do.
#[derive(Debug)]
pub enum ServeStart {
    /// Boot the engine with this plan.
    Boot(ServePlan),
    /// A gardend already serves this profile. Print this and exit 0.
    AlreadyRunning(String),
}

#[derive(Debug, Clone)]
pub struct ServePlan {
    pub profile_dir: PathBuf,
    pub port: u16,
    pub host: IpAddr,
    pub key_source: KeySource,
    pub ui_dir: Option<PathBuf>,
    pub warnings: Vec<String>,
    key: String,
    /// What `gardend url` needs to be told to find this profile again.
    url_command: String,
    /// Where a `ui/` folder would be picked up from.
    ui_hint_dir: Option<PathBuf>,
    /// Process environment the engine must start with.
    env: Vec<(String, String)>,
}

/// Decide how to serve from the real process environment and binary path.
pub fn prepare_serve(args: &ServeArgs) -> Result<ServeStart, String> {
    let exe = std::env::current_exe().ok();
    plan_serve(args, std::env::consts::OS, &process_env, exe.as_deref(), true)
}

pub(crate) fn plan_serve(
    args: &ServeArgs,
    os: &str,
    env: Env,
    exe: Option<&Path>,
    probe_port: bool,
) -> Result<ServeStart, String> {
    let profile_dir = resolve_profile_dir(args.profile.as_deref(), os, env)?;
    let port = resolve_port(args.port, env)?;
    let (host, host_warning) = resolve_host(env)?;
    let url_command = url_command(&profile_dir, os, env);
    ensure_profile_dir(&profile_dir)?;

    if let Some(running) = serving_now(&profile_dir) {
        let url = open_url(&link_host(host), running.port, &running.token);
        return Ok(ServeStart::AlreadyRunning(format!(
            "Sophia is already running for {} (gardend pid {}).\n  Open Sophia: {}",
            profile_dir.display(),
            running.pid,
            shown_url(&url, std::io::IsTerminal::is_terminal(&std::io::stderr()), &url_command),
        )));
    }

    let (key, key_source) = match env(ENV_TOKEN) {
        Some(token) => (token, KeySource::Environment),
        None => ensure_profile_key(&profile_dir)?,
    };

    if probe_port && port != 0 {
        if let Err(error) = std::net::TcpListener::bind((host, port)) {
            return Err(format!(
                "port {port} on {host} is not free ({error}). Another program, or another \
                 Sophia, holds it: pick another with --port <n>."
            ));
        }
    }

    let (ui_dir, ui_from_env) = match env(ENV_UI_DIR) {
        Some(dir) => (Some(PathBuf::from(dir)), true),
        None => (default_ui_dir(exe), false),
    };
    let mut engine_env = vec![
        (ENV_PROFILE_DIR.to_string(), profile_dir.display().to_string()),
        (ENV_PORT.to_string(), port.to_string()),
        (ENV_TOKEN.to_string(), key.clone()),
    ];
    if !ui_from_env {
        if let Some(dir) = &ui_dir {
            engine_env.push((ENV_UI_DIR.to_string(), dir.display().to_string()));
        }
    }
    // A quiet terminal by default, so the open link stands out; RUST_LOG=info
    // brings the engine's full log back.
    if env("RUST_LOG").is_none() {
        engine_env.push(("RUST_LOG".to_string(), "warn,gardend=info".to_string()));
    }

    Ok(ServeStart::Boot(ServePlan {
        profile_dir,
        port,
        host,
        key_source,
        ui_dir,
        warnings: host_warning.into_iter().collect(),
        key,
        url_command,
        ui_hint_dir: exe.and_then(Path::parent).map(|dir| dir.join("ui")),
        env: engine_env,
    }))
}

/// `gardend url`, with `--profile` only when the profile is not the default.
fn url_command(profile_dir: &Path, os: &str, env: Env) -> String {
    let default = default_profile_dir(os, env).ok();
    if env(ENV_PROFILE_DIR).is_none() && default.as_deref() == Some(profile_dir) {
        "gardend url".to_string()
    } else {
        format!("gardend url --profile \"{}\"", profile_dir.display())
    }
}

/// The open link as a terminal shows it; anywhere else (a service log) the
/// key is left out and the reader is told how to print the whole link.
pub(crate) fn shown_url(url: &str, interactive: bool, url_command: &str) -> String {
    if interactive {
        return url.to_string();
    }
    match url.split_once("#key=") {
        Some((base, _)) => format!("{base}#key=… (key withheld from logs; `{url_command}` prints the full link)"),
        None => url.to_string(),
    }
}

impl ServePlan {
    /// Put the plan into the process environment. Call before the engine
    /// starts any thread.
    pub fn apply_env(&self) {
        for (key, value) in &self.env {
            std::env::set_var(key, value);
        }
    }

    pub fn open_url(&self, port: u16) -> String {
        open_url(&link_host(self.host), port, &self.key)
    }

    /// The port actually bound (it differs from the plan only for `--port 0`).
    fn bound_port(&self) -> u16 {
        if self.port != 0 {
            return self.port;
        }
        read_running_manifest(&self.profile_dir)
            .map(|manifest| manifest.port)
            .unwrap_or(self.port)
    }

    /// Printed on stderr once the API answers. `interactive`: stderr is a
    /// terminal (the key is shown only there).
    pub fn ready_banner(&self, interactive: bool) -> String {
        let port = self.bound_port();
        let url = self.open_url(port);
        let base = format!("http://{}:{port}", link_host(self.host));
        let ui = match &self.ui_dir {
            Some(dir) => dir.display().to_string(),
            None => format!(
                "none found: API and MCP only (put the Sophia UI in {}, or set {ENV_UI_DIR})",
                self.ui_hint_dir
                    .as_ref()
                    .map(|dir| dir.display().to_string())
                    .unwrap_or_else(|| "ui/ next to gardend".to_string())
            ),
        };
        let key = match self.key_source {
            KeySource::Environment => format!("from {ENV_TOKEN}"),
            KeySource::ProfileFile | KeySource::Minted => {
                self.profile_dir.join(KEY_FILE).display().to_string()
            }
        };
        let mut out = String::new();
        for warning in &self.warnings {
            out.push_str(warning);
            out.push_str("\n\n");
        }
        out.push_str("Sophia is ready.\n\n");
        out.push_str(&format!(
            "  Open Sophia: {}\n\n",
            shown_url(&url, interactive, &self.url_command)
        ));
        out.push_str(&format!("  profile  {}\n", self.profile_dir.display()));
        out.push_str(&format!("  ui       {ui}\n"));
        out.push_str(&format!("  mcp      {base}/mcp (Authorization: Bearer <key>)\n"));
        out.push_str(&format!("  key      {key}\n\n"));
        out.push_str(
            "The key in that link opens everything in this profile; keep it to yourself.\n\
             Give other people and agents their own token:\n  \
             gardend token create --label <name> --profile authoring\n\
             Stop with Ctrl-C (gardend flushes before it exits). Keep it running in the \
             background: gardend service install\n",
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{env_of, tempdir};
    use super::*;

    fn boot(start: ServeStart) -> ServePlan {
        match start {
            ServeStart::Boot(plan) => plan,
            ServeStart::AlreadyRunning(message) => panic!("unexpected: {message}"),
        }
    }

    fn env_value<'a>(plan: &'a ServePlan, key: &str) -> Option<&'a str> {
        plan.env
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn serve_plan_pins_profile_port_and_a_stable_key() {
        let root = tempdir("plan");
        let profile = root.join("profile");
        let env = env_of(&[("HOME", "/nonexistent-home")]);
        let args = ServeArgs {
            profile: Some(profile.display().to_string()),
            port: None,
        };
        let plan = boot(plan_serve(&args, "linux", &env, None, false).unwrap());
        assert_eq!(plan.profile_dir, profile);
        assert_eq!(plan.port, super::super::DEFAULT_PORT);
        assert_eq!(plan.key_source, KeySource::Minted);
        assert_eq!(env_value(&plan, "GARDEN_PROFILE_DIR"), Some(profile.to_str().unwrap()));
        assert_eq!(env_value(&plan, "GARDEN_LOOPBACK_PORT"), Some("7674"));
        let key = env_value(&plan, "GARDEN_LOOPBACK_TOKEN").unwrap().to_string();
        assert!(key.starts_with("sophia-key-"));
        assert_eq!(plan.open_url(7674), format!("http://127.0.0.1:7674/#key={key}"));
        assert!(env_value(&plan, "GARDEN_UI_DIR").is_none());
        assert_eq!(env_value(&plan, "RUST_LOG"), Some("warn,gardend=info"));

        // A restart keeps the link.
        let again = boot(plan_serve(&args, "linux", &env, None, false).unwrap());
        assert_eq!(again.key_source, KeySource::ProfileFile);
        assert_eq!(env_value(&again, "GARDEN_LOOPBACK_TOKEN"), Some(key.as_str()));
    }

    #[test]
    fn an_explicit_token_wins_and_is_not_stored() {
        let profile = tempdir("envtoken").join("p");
        let env = env_of(&[
            ("HOME", "/nonexistent-home"),
            ("GARDEN_PROFILE_DIR", profile.to_str().unwrap()),
            ("GARDEN_LOOPBACK_TOKEN", "operator-chosen"),
            ("GARDEN_LOOPBACK_PORT", "7801"),
        ]);
        let plan = boot(plan_serve(&ServeArgs::default(), "linux", &env, None, false).unwrap());
        assert_eq!(plan.profile_dir, profile);
        assert_eq!(plan.port, 7801);
        assert_eq!(plan.key_source, KeySource::Environment);
        assert_eq!(plan.open_url(7801), "http://127.0.0.1:7801/#key=operator-chosen");
        assert!(!profile.join(KEY_FILE).exists());
    }

    #[test]
    fn a_ui_folder_next_to_the_binary_becomes_garden_ui_dir() {
        let root = tempdir("ui-plan");
        let exe = root.join("gardend");
        std::fs::write(&exe, b"").unwrap();
        std::fs::create_dir_all(root.join("ui")).unwrap();
        std::fs::write(root.join("ui/index.html"), b"<!doctype html>").unwrap();
        let env = env_of(&[("HOME", "/nonexistent-home")]);
        let args = ServeArgs {
            profile: Some(root.join("p").display().to_string()),
            port: Some(7802),
        };
        let plan = boot(plan_serve(&args, "linux", &env, Some(&exe), false).unwrap());
        assert_eq!(plan.ui_dir, Some(root.join("ui")));
        assert_eq!(
            env_value(&plan, "GARDEN_UI_DIR"),
            Some(root.join("ui").to_str().unwrap())
        );

        // An operator's GARDEN_UI_DIR is left to A's server as is.
        let env = env_of(&[("HOME", "/nonexistent-home"), ("GARDEN_UI_DIR", "/opt/ui")]);
        let plan = boot(plan_serve(&args, "linux", &env, Some(&exe), false).unwrap());
        assert_eq!(plan.ui_dir, Some(PathBuf::from("/opt/ui")));
        assert!(env_value(&plan, "GARDEN_UI_DIR").is_none());
    }

    #[test]
    fn banner_shows_the_key_only_on_a_terminal() {
        let root = tempdir("banner");
        let env = env_of(&[("HOME", "/nonexistent-home")]);
        let args = ServeArgs {
            profile: Some(root.join("p").display().to_string()),
            port: Some(7803),
        };
        let plan = boot(plan_serve(&args, "linux", &env, None, false).unwrap());
        let key = env_value(&plan, "GARDEN_LOOPBACK_TOKEN").unwrap().to_string();
        let terminal = plan.ready_banner(true);
        assert!(terminal.contains(&format!("  Open Sophia: http://127.0.0.1:7803/#key={key}\n")));
        assert!(terminal.contains("none found: API and MCP only"));
        let log = plan.ready_banner(false);
        assert!(!log.contains(&key), "a service log must not carry the key");
        assert!(log.contains("Open Sophia: http://127.0.0.1:7803/#key=…"));
        assert!(log.contains("gardend url --profile"));
    }

    #[test]
    fn a_non_loopback_bind_warns_in_the_banner() {
        let root = tempdir("host");
        let env = env_of(&[("HOME", "/nonexistent-home"), ("GARDEN_LOOPBACK_HOST", "0.0.0.0")]);
        let args = ServeArgs {
            profile: Some(root.join("p").display().to_string()),
            port: Some(7804),
        };
        let plan = boot(plan_serve(&args, "linux", &env, None, false).unwrap());
        assert_eq!(plan.warnings.len(), 1);
        assert!(plan.ready_banner(true).starts_with("WARNING: GARDEN_LOOPBACK_HOST=0.0.0.0"));
        assert!(plan.open_url(7804).starts_with("http://127.0.0.1:7804/"));
    }

    #[test]
    fn a_taken_port_is_refused_before_boot() {
        let holder = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = holder.local_addr().unwrap().port();
        let root = tempdir("busy");
        let env = env_of(&[("HOME", "/nonexistent-home")]);
        let args = ServeArgs {
            profile: Some(root.join("p").display().to_string()),
            port: Some(port),
        };
        let error = plan_serve(&args, "linux", &env, None, true).unwrap_err();
        assert!(error.contains(&format!("port {port}")), "{error}");
        assert!(error.contains("--port"));
    }
}
