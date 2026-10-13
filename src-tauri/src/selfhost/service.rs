//! `gardend service install|uninstall|status`: keep `gardend serve` running
//! as the user, from login on.
//!
//! - macOS: a launchd LaunchAgent, `~/Library/LaunchAgents/dev.sophia.gardend.plist`,
//!   logging to `~/Library/Logs/Sophia/gardend.log`.
//! - Linux: a systemd user unit, `~/.config/systemd/user/gardend.service`,
//!   logging to the journal (`journalctl --user -u gardend`).
//! - Windows: not yet.

use super::{
    ensure_profile_dir, ensure_profile_key, open_url, read_running_manifest, resolve_port,
    resolve_profile_dir, serving_now, Env, KEY_FILE,
};
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) const LAUNCHD_LABEL: &str = "dev.sophia.gardend";
pub(crate) const SYSTEMD_UNIT: &str = "gardend.service";

/// Knobs copied from the installing shell into the service, so a service
/// runs with the same UI, Cloudflare Access and logging settings as a
/// `gardend serve` typed in that shell. Never the key: the service reads it
/// from the profile.
pub(crate) const CARRIED_ENV: &[&str] = &[
    "GARDEN_UI_DIR",
    "GARDEN_CF_ACCESS_TEAM_DOMAIN",
    "GARDEN_CF_ACCESS_AUD",
    "GARDEN_CF_ACCESS_ADMIN_EMAILS",
    "GARDEN_ALLOWED_ORIGINS",
    "GARDEN_PUBLIC_BASE_URL",
    "GARDEN_LOOPBACK_HOST",
    "GARDEN_NO_UPDATE_CHECK",
    "RUST_LOG",
    "PATH",
];

const WINDOWS_NOT_YET: &str = "gardend service is not available on Windows yet. Run \
    `gardend serve` in a terminal, or start it at log-on from Task Scheduler.";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServiceArgs {
    pub profile: Option<String>,
    pub port: Option<u16>,
    /// Print the file that would be installed, change nothing.
    pub print: bool,
}

/// What the service runs.
#[derive(Debug, Clone)]
pub(crate) struct ServiceSpec {
    pub(crate) exe: PathBuf,
    pub(crate) profile_dir: PathBuf,
    pub(crate) port: u16,
    pub(crate) env: Vec<(String, String)>,
}

impl ServiceSpec {
    fn args(&self) -> Vec<String> {
        vec![
            self.exe.display().to_string(),
            "serve".to_string(),
            "--profile".to_string(),
            self.profile_dir.display().to_string(),
            "--port".to_string(),
            self.port.to_string(),
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Manager {
    Launchd,
    Systemd,
}

fn manager() -> Result<Manager, String> {
    if cfg!(target_os = "macos") {
        Ok(Manager::Launchd)
    } else if cfg!(windows) {
        Err(WINDOWS_NOT_YET.to_string())
    } else if cfg!(target_os = "linux") {
        Ok(Manager::Systemd)
    } else {
        Err(format!(
            "gardend service knows launchd (macOS) and systemd (Linux), not {}. Run `gardend serve` \
             under your own supervisor.",
            std::env::consts::OS
        ))
    }
}

fn home(env: Env) -> Result<PathBuf, String> {
    env("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "HOME is not set".to_string())
}

/// Where the service definition lives.
pub(crate) fn unit_path(manager: Manager, env: Env) -> Result<PathBuf, String> {
    Ok(match manager {
        Manager::Launchd => home(env)?
            .join("Library/LaunchAgents")
            .join(format!("{LAUNCHD_LABEL}.plist")),
        Manager::Systemd => match env("XDG_CONFIG_HOME") {
            Some(dir) => PathBuf::from(dir),
            None => home(env)?.join(".config"),
        }
        .join("systemd/user")
        .join(SYSTEMD_UNIT),
    })
}

/// Where the service's output goes, as a person reads it.
pub(crate) fn log_location(manager: Manager, env: Env) -> Result<String, String> {
    Ok(match manager {
        Manager::Launchd => launchd_log_path(env)?.display().to_string(),
        Manager::Systemd => "the user journal: journalctl --user -u gardend -f".to_string(),
    })
}

fn launchd_log_path(env: Env) -> Result<PathBuf, String> {
    Ok(home(env)?.join("Library/Logs/Sophia/gardend.log"))
}

// ---- rendering ---------------------------------------------------------

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn xml_unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

pub(crate) fn render_launchd_plist(spec: &ServiceSpec, log_path: &Path) -> String {
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <!-- Installed by `gardend service install`; remove it with `gardend service uninstall`. -->\n\
         <plist version=\"1.0\">\n<dict>\n",
    );
    out.push_str(&format!(
        "\t<key>Label</key>\n\t<string>{LAUNCHD_LABEL}</string>\n\t<key>ProgramArguments</key>\n\t<array>\n"
    ));
    for arg in spec.args() {
        out.push_str(&format!("\t\t<string>{}</string>\n", xml_escape(&arg)));
    }
    out.push_str("\t</array>\n");
    if !spec.env.is_empty() {
        out.push_str("\t<key>EnvironmentVariables</key>\n\t<dict>\n");
        for (key, value) in &spec.env {
            out.push_str(&format!(
                "\t\t<key>{}</key>\n\t\t<string>{}</string>\n",
                xml_escape(key),
                xml_escape(value)
            ));
        }
        out.push_str("\t</dict>\n");
    }
    let log = xml_escape(&log_path.display().to_string());
    // KeepAlive/SuccessfulExit=false: restart after a crash, not after a
    // clean stop. ExitTimeOut gives the final durable flush time before
    // launchd escalates to SIGKILL.
    out.push_str(&format!(
        "\t<key>RunAtLoad</key>\n\t<true/>\n\
         \t<key>KeepAlive</key>\n\t<dict>\n\t\t<key>SuccessfulExit</key>\n\t\t<false/>\n\t</dict>\n\
         \t<key>ThrottleInterval</key>\n\t<integer>10</integer>\n\
         \t<key>ExitTimeOut</key>\n\t<integer>60</integer>\n\
         \t<key>StandardOutPath</key>\n\t<string>{log}</string>\n\
         \t<key>StandardErrorPath</key>\n\t<string>{log}</string>\n\
         </dict>\n</plist>\n"
    ));
    out
}

/// One `ExecStart=` word: `\` and `"` escaped inside quotes, `%` (specifiers)
/// and `$` (variable expansion) doubled.
fn systemd_word(word: &str) -> String {
    let plain = !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-=:+,@".contains(c));
    let doubled = word.replace('%', "%%").replace('$', "$$");
    if plain {
        doubled
    } else {
        format!(
            "\"{}\"",
            doubled.replace('\\', "\\\\").replace('"', "\\\"")
        )
    }
}

/// An `Environment=` assignment: quoted, `%` doubled (`$` is not expanded there).
fn systemd_env(key: &str, value: &str) -> String {
    let assignment = format!("{key}={value}")
        .replace('%', "%%")
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    format!("Environment=\"{assignment}\"")
}

pub(crate) fn render_systemd_unit(spec: &ServiceSpec) -> String {
    let exec = spec
        .args()
        .iter()
        .map(|word| systemd_word(word))
        .collect::<Vec<_>>()
        .join(" ");
    let mut out = String::from(
        "# Installed by `gardend service install`; remove it with `gardend service uninstall`.\n\
         # Logs: journalctl --user -u gardend -f\n\
         [Unit]\n\
         Description=Sophia (gardend serve)\n\
         Documentation=https://github.com/sophia-labs/gardend\n\
         \n\
         [Service]\n\
         Type=simple\n",
    );
    out.push_str(&format!("ExecStart={exec}\n"));
    for (key, value) in &spec.env {
        out.push_str(&systemd_env(key, value));
        out.push('\n');
    }
    // Restart after a crash, not after a clean stop; give the final durable
    // flush time before SIGKILL.
    out.push_str(
        "Restart=on-failure\n\
         RestartSec=5\n\
         TimeoutStopSec=60\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
    );
    out
}

/// The `ProgramArguments` of a plist this module wrote.
pub(crate) fn parse_launchd_args(plist: &str) -> Vec<String> {
    let Some(start) = plist.find("<key>ProgramArguments</key>") else {
        return Vec::new();
    };
    let rest = &plist[start..];
    let end = rest.find("</array>").unwrap_or(rest.len());
    let mut args = Vec::new();
    let mut section = &rest[..end];
    while let Some(open) = section.find("<string>") {
        let after = &section[open + "<string>".len()..];
        let Some(close) = after.find("</string>") else {
            break;
        };
        args.push(xml_unescape(&after[..close]));
        section = &after[close + "</string>".len()..];
    }
    args
}

/// The words of the `ExecStart=` line of a unit this module wrote.
pub(crate) fn parse_systemd_exec_start(unit: &str) -> Vec<String> {
    let Some(line) = unit
        .lines()
        .find_map(|line| line.trim().strip_prefix("ExecStart="))
    else {
        return Vec::new();
    };
    let mut words = Vec::new();
    let mut chars = line.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let Some(first) = chars.next() else {
            break;
        };
        let mut word = String::new();
        if first == '"' {
            while let Some(c) = chars.next() {
                match c {
                    '\\' => {
                        if let Some(escaped) = chars.next() {
                            word.push(escaped);
                        }
                    }
                    '"' => break,
                    other => word.push(other),
                }
            }
        } else {
            word.push(first);
            while let Some(c) = chars.peek().copied() {
                if c.is_whitespace() {
                    break;
                }
                word.push(c);
                chars.next();
            }
        }
        words.push(word.replace("%%", "%").replace("$$", "$"));
    }
    words
}

/// `--profile` and `--port` out of an installed command line.
pub(crate) fn installed_profile_and_port(args: &[String]) -> (Option<PathBuf>, Option<u16>) {
    let mut profile = None;
    let mut port = None;
    for pair in args.windows(2) {
        match pair[0].as_str() {
            "--profile" => profile = Some(PathBuf::from(&pair[1])),
            "--port" => port = pair[1].parse().ok(),
            _ => {}
        }
    }
    (profile, port)
}

fn read_installed(manager: Manager, env: Env) -> Result<Option<(PathBuf, Vec<String>)>, String> {
    let path = unit_path(manager, env)?;
    if !path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    let args = match manager {
        Manager::Launchd => parse_launchd_args(&text),
        Manager::Systemd => parse_systemd_exec_start(&text),
    };
    Ok(Some((path, args)))
}

// ---- acting ------------------------------------------------------------

fn run(program: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| format!("run {program}: {error}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if output.status.success() {
        Ok(stdout)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "{program} {} failed ({}): {}{}",
            args.join(" "),
            output.status,
            stderr.trim(),
            stdout.trim()
        ))
    }
}

fn gui_domain() -> Result<String, String> {
    let uid = run("id", &["-u"])?;
    Ok(format!("gui/{}", uid.trim()))
}

fn carried_env(env: Env) -> Vec<(String, String)> {
    CARRIED_ENV
        .iter()
        .filter_map(|key| env(key).map(|value| (key.to_string(), value)))
        .collect()
}

pub(crate) fn install(args: &ServiceArgs, env: Env) -> Result<String, String> {
    let manager = manager()?;
    let profile_dir = resolve_profile_dir(args.profile.as_deref(), std::env::consts::OS, env)?;
    let port = resolve_port(args.port, env)?;
    let exe = std::env::current_exe()
        .map_err(|error| format!("cannot locate the gardend binary: {error}"))?;
    let spec = ServiceSpec {
        exe,
        profile_dir,
        port,
        env: carried_env(env),
    };
    let path = unit_path(manager, env)?;
    let text = match manager {
        Manager::Launchd => render_launchd_plist(&spec, &launchd_log_path(env)?),
        Manager::Systemd => render_systemd_unit(&spec),
    };
    if args.print {
        return Ok(format!("# {}\n{text}", path.display()));
    }

    ensure_profile_dir(&spec.profile_dir)?;
    let (key, _) = ensure_profile_key(&spec.profile_dir)?;
    let mut notes = Vec::new();
    if let Some(running) = serving_now(&spec.profile_dir) {
        notes.push(format!(
            "note: a gardend (pid {}) already serves this profile. Stop it; the service takes over \
             once the profile is free.",
            running.pid
        ));
    }
    if env("GARDEN_LOOPBACK_TOKEN").is_some() {
        notes.push(
            "note: GARDEN_LOOPBACK_TOKEN is not carried into the service; it uses the profile's key."
                .to_string(),
        );
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    std::fs::write(&path, &text).map_err(|error| format!("write {}: {error}", path.display()))?;

    match manager {
        Manager::Launchd => {
            let log_dir = launchd_log_path(env)?
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default();
            ensure_profile_dir(&log_dir)?; // owner-only, like the profile
            let domain = gui_domain()?;
            let _ = run("launchctl", &["bootout", &format!("{domain}/{LAUNCHD_LABEL}")]);
            let plist = path.display().to_string();
            let mut last = Ok(String::new());
            // Right after a bootout launchd can briefly refuse the same label.
            for attempt in 0..5 {
                last = run("launchctl", &["bootstrap", &domain, &plist]);
                if last.is_ok() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(500 * (attempt + 1)));
            }
            last?;
        }
        Manager::Systemd => {
            run("systemctl", &["--user", "daemon-reload"])?;
            run("systemctl", &["--user", "enable", SYSTEMD_UNIT])?;
            run("systemctl", &["--user", "restart", SYSTEMD_UNIT])?;
            if let Some(user) = env("USER") {
                let linger = run(
                    "loginctl",
                    &["show-user", &user, "--property=Linger", "--value"],
                )
                .unwrap_or_default();
                if linger.trim() == "no" {
                    notes.push(format!(
                        "note: it runs while you are logged in. To keep it running after you log \
                         out: sudo loginctl enable-linger {user}"
                    ));
                }
            }
        }
    }

    let carried = if spec.env.is_empty() {
        "nothing".to_string()
    } else {
        spec.env
            .iter()
            .map(|(key, _)| key.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut out = format!(
        "Installed the Sophia service ({}).\n\
         \n  file     {}\n  runs     {}\n  profile  {}\n  logs     {}\n  env      {carried} (from this shell)\n\n\
         It starts now and at every login. In a few seconds:\n\n  Open Sophia: {}\n\n\
         Check it: gardend service status. Remove it: gardend service uninstall (your profile stays).\n",
        match manager {
            Manager::Launchd => format!("launchd agent {LAUNCHD_LABEL}"),
            Manager::Systemd => format!("systemd user unit {SYSTEMD_UNIT}"),
        },
        path.display(),
        spec.args().join(" "),
        spec.profile_dir.display(),
        log_location(manager, env)?,
        open_url("127.0.0.1", port, &key),
    );
    for note in notes {
        out.push('\n');
        out.push_str(&note);
        out.push('\n');
    }
    Ok(out)
}

pub(crate) fn uninstall(env: Env) -> Result<String, String> {
    let manager = manager()?;
    let Some((path, args)) = read_installed(manager, env)? else {
        return Ok(format!(
            "The Sophia service is not installed (no {}).\n",
            unit_path(manager, env)?.display()
        ));
    };
    match manager {
        Manager::Launchd => {
            let domain = gui_domain()?;
            let _ = run("launchctl", &["bootout", &format!("{domain}/{LAUNCHD_LABEL}")]);
            std::fs::remove_file(&path)
                .map_err(|error| format!("remove {}: {error}", path.display()))?;
        }
        Manager::Systemd => {
            let _ = run("systemctl", &["--user", "disable", "--now", SYSTEMD_UNIT]);
            std::fs::remove_file(&path)
                .map_err(|error| format!("remove {}: {error}", path.display()))?;
            let _ = run("systemctl", &["--user", "daemon-reload"]);
        }
    }
    let (profile, _) = installed_profile_and_port(&args);
    Ok(format!(
        "Stopped and removed the Sophia service ({}).{}\n",
        path.display(),
        profile
            .map(|dir| format!(" Your profile is untouched: {}", dir.display()))
            .unwrap_or_default()
    ))
}

/// The report, and whether the service is running.
pub(crate) fn status(env: Env) -> Result<(String, bool), String> {
    let manager = manager()?;
    let Some((path, args)) = read_installed(manager, env)? else {
        return Ok((
            format!(
                "The Sophia service is not installed (no {}). Install it with: gardend service install\n",
                unit_path(manager, env)?.display()
            ),
            false,
        ));
    };
    let (state, pid) = match manager {
        Manager::Launchd => {
            let domain = gui_domain()?;
            match run("launchctl", &["print", &format!("{domain}/{LAUNCHD_LABEL}")]) {
                Ok(text) => {
                    let field = |name: &str| {
                        text.lines().find_map(|line| {
                            line.trim()
                                .strip_prefix(name)
                                .and_then(|rest| rest.trim_start().strip_prefix('='))
                                .map(|value| value.trim().to_string())
                        })
                    };
                    (
                        field("state").unwrap_or_else(|| "loaded".to_string()),
                        field("pid"),
                    )
                }
                Err(_) => ("not loaded".to_string(), None),
            }
        }
        Manager::Systemd => {
            let state = match Command::new("systemctl")
                .args(["--user", "is-active", SYSTEMD_UNIT])
                .output()
            {
                Ok(output) => String::from_utf8_lossy(&output.stdout).trim().to_string(),
                Err(error) => format!("unknown ({error})"),
            };
            let pid = run(
                "systemctl",
                &["--user", "show", SYSTEMD_UNIT, "--property=MainPID", "--value"],
            )
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty() && value != "0");
            (state, pid)
        }
    };
    let running = matches!(state.as_str(), "running" | "active");
    let (profile, port) = installed_profile_and_port(&args);
    let mut out = format!(
        "Sophia service: {state}{}\n  file     {}\n  runs     {}\n  logs     {}\n",
        pid.map(|pid| format!(" (pid {pid})")).unwrap_or_default(),
        path.display(),
        args.join(" "),
        log_location(manager, env)?,
    );
    if let Some(profile) = profile {
        out.push_str(&format!("  profile  {}\n", profile.display()));
        let link = match read_running_manifest(&profile) {
            Some(manifest) if running && !manifest.token.is_empty() => {
                Some(open_url("127.0.0.1", manifest.port, &manifest.token))
            }
            _ => std::fs::read_to_string(profile.join(KEY_FILE))
                .ok()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
                .and_then(|value| value.get("key")?.as_str().map(str::to_string))
                .map(|key| open_url("127.0.0.1", port.unwrap_or(super::DEFAULT_PORT), &key)),
        };
        if let Some(link) = link {
            out.push_str(&format!("\n  Open Sophia: {link}\n"));
        }
    }
    Ok((out, running))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::env_of;
    use super::*;

    fn spec() -> ServiceSpec {
        ServiceSpec {
            exe: PathBuf::from("/opt/gardend/gardend"),
            profile_dir: PathBuf::from("/Users/ana/Library/Application Support/Sophia & Co"),
            port: 7674,
            env: vec![
                ("GARDEN_UI_DIR".to_string(), "/opt/gardend/ui".to_string()),
                ("RUST_LOG".to_string(), "info,garden=debug".to_string()),
            ],
        }
    }

    #[test]
    fn launchd_plist_runs_serve_with_profile_port_and_logs() {
        let plist = render_launchd_plist(&spec(), Path::new("/Users/ana/Library/Logs/Sophia/gardend.log"));
        assert!(plist.contains("<string>dev.sophia.gardend</string>"));
        assert!(plist.contains("<string>Sophia &amp; Co</string>") || plist.contains("Sophia &amp; Co</string>"));
        assert!(!plist.contains("Sophia & Co"), "XML must be escaped");
        assert!(plist.contains("<key>GARDEN_UI_DIR</key>\n\t\t<string>/opt/gardend/ui</string>"));
        assert!(plist.contains("<key>StandardErrorPath</key>\n\t<string>/Users/ana/Library/Logs/Sophia/gardend.log</string>"));
        assert!(plist.contains("<key>SuccessfulExit</key>\n\t\t<false/>"));
        assert!(plist.contains("<key>ExitTimeOut</key>\n\t<integer>60</integer>"));
        let args = parse_launchd_args(&plist);
        assert_eq!(
            args,
            vec![
                "/opt/gardend/gardend",
                "serve",
                "--profile",
                "/Users/ana/Library/Application Support/Sophia & Co",
                "--port",
                "7674"
            ]
        );
        let (profile, port) = installed_profile_and_port(&args);
        assert_eq!(profile, Some(spec().profile_dir));
        assert_eq!(port, Some(7674));
    }

    #[test]
    fn systemd_unit_quotes_and_round_trips() {
        let mut spec = spec();
        spec.profile_dir = PathBuf::from("/home/ana/my \"sophia\" 100%");
        let unit = render_systemd_unit(&spec);
        assert!(unit.contains(
            "ExecStart=/opt/gardend/gardend serve --profile \"/home/ana/my \\\"sophia\\\" 100%%\" --port 7674\n"
        ));
        assert!(unit.contains("Environment=\"RUST_LOG=info,garden=debug\"\n"));
        assert!(unit.contains("Restart=on-failure\n"));
        assert!(unit.contains("WantedBy=default.target\n"));
        let args = parse_systemd_exec_start(&unit);
        assert_eq!(args[3], "/home/ana/my \"sophia\" 100%");
        assert_eq!(installed_profile_and_port(&args), (Some(spec.profile_dir.clone()), Some(7674)));
        assert_eq!(systemd_word("$HOME"), "\"$$HOME\"");
    }

    #[test]
    fn unit_paths_follow_the_platform_conventions() {
        let env = env_of(&[("HOME", "/home/ana")]);
        assert_eq!(
            unit_path(Manager::Launchd, &env).unwrap(),
            PathBuf::from("/home/ana/Library/LaunchAgents/dev.sophia.gardend.plist")
        );
        assert_eq!(
            unit_path(Manager::Systemd, &env).unwrap(),
            PathBuf::from("/home/ana/.config/systemd/user/gardend.service")
        );
        let xdg = env_of(&[("HOME", "/home/ana"), ("XDG_CONFIG_HOME", "/cfg")]);
        assert_eq!(
            unit_path(Manager::Systemd, &xdg).unwrap(),
            PathBuf::from("/cfg/systemd/user/gardend.service")
        );
        assert_eq!(
            launchd_log_path(&env).unwrap(),
            PathBuf::from("/home/ana/Library/Logs/Sophia/gardend.log")
        );
    }

    #[test]
    fn only_listed_knobs_are_carried_and_never_the_key() {
        let env = env_of(&[
            ("GARDEN_CF_ACCESS_AUD", "aud-123"),
            ("GARDEN_LOOPBACK_TOKEN", "secret"),
            ("AWS_SECRET_ACCESS_KEY", "nope"),
            ("PATH", "/usr/bin:/bin"),
        ]);
        let carried = carried_env(&env);
        assert_eq!(
            carried,
            vec![
                ("GARDEN_CF_ACCESS_AUD".to_string(), "aud-123".to_string()),
                ("PATH".to_string(), "/usr/bin:/bin".to_string()),
            ]
        );
    }

    #[test]
    fn install_print_changes_nothing() {
        let root = super::super::test_support::tempdir("service-print");
        let home = root.join("home");
        let env = env_of(&[("HOME", home.to_str().unwrap()), ("XDG_CONFIG_HOME", root.join("cfg").to_str().unwrap())]);
        let args = ServiceArgs {
            profile: Some(root.join("profile").display().to_string()),
            port: Some(7805),
            print: true,
        };
        match install(&args, &env) {
            Ok(text) => {
                assert!(text.contains("serve"));
                assert!(text.contains("7805"));
                assert!(!root.join("profile").exists(), "--print must not create the profile");
                assert!(!home.exists() && !root.join("cfg").exists(), "--print must not write a unit");
            }
            // Platforms without a service manager say so instead.
            Err(error) => assert!(error.contains("not available") || error.contains("knows launchd")),
        }
    }
}
