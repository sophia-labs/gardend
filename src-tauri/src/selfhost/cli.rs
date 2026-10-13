//! Argument parsing and dispatch for the self-host verbs. `parse` answers
//! `None` for anything that is not one of them, so plain `gardend` (and the
//! older `--version` / `--check-update` / `update`) keeps its old path.

use super::service::{self, ServiceArgs};
use super::tokens::{self, TokenCreateArgs, GRANT_PROFILES};
use super::{
    ensure_profile_dir, ensure_profile_key, open_url, process_env, resolve_port,
    resolve_profile_dir, serving_now, Env,
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ServeArgs {
    /// `--profile <dir>` (or `--profile-dir <dir>`).
    pub profile: Option<String>,
    pub port: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Help(Option<String>),
    Serve(ServeArgs),
    Url(ServeArgs),
    TokenCreate(TokenCreateArgs),
    TokenList { profile_dir: Option<String>, json: bool },
    TokenRevoke { id: String, profile_dir: Option<String> },
    ServiceInstall(ServiceArgs),
    ServiceUninstall,
    ServiceStatus,
}

const HELP: &str = "\
gardend: the Sophia engine. Run your own Sophia with it.

  gardend serve [--profile <dir>] [--port <n>]
      Run Sophia on http://127.0.0.1:7674 and print the link to open.
  gardend url [--profile <dir>] [--port <n>]
      Print that link again (it carries the profile's key).
  gardend token create --label <name> [--grant read-only|authoring|member|rdf-admin|session-all]
                       [--expires-days <n>] [--profile-dir <dir>] [--json]
  gardend token list [--profile-dir <dir>] [--json]
  gardend token revoke <id> [--profile-dir <dir>]
      Scoped, expiring tokens for other people, devices and agents
      (give agents --grant member).
  gardend service install [--profile <dir>] [--port <n>] [--print]
  gardend service uninstall | status
      Keep `gardend serve` running in the background (launchd on macOS,
      systemd --user on Linux).
  gardend --version [--json] | --check-update | update

The profile directory is --profile (--profile-dir for `token`), else
GARDEN_PROFILE_DIR, else ~/Library/Application Support/Sophia (macOS),
~/.local/share/sophia (Linux) or %LOCALAPPDATA%\\Sophia (Windows).
`gardend <verb> --help` says more. With no verb, gardend runs as a hosted cell,
configured by GARDEN_* environment variables.
";

const SERVE_HELP: &str = "\
gardend serve [--profile <dir>] [--port <n>]

Runs Sophia for one profile on 127.0.0.1 and, once it is ready, prints

  Open Sophia: http://127.0.0.1:<port>/#key=<key>

  --profile <dir>  the profile (default: GARDEN_PROFILE_DIR, else the per-OS
                   default; see `gardend --help`). Created if missing. An
                   unpacked \"Download my gardens\" folder is a profile.
  --port <n>       default GARDEN_LOOPBACK_PORT, else 7674.

The key is the profile's master key, kept in <profile>/selfhost-key.json so
the link survives restarts (delete the file and restart to rotate it). It opens
everything; give agents and other people `gardend token create --grant member`
tokens instead. A `ui/` folder next to the gardend binary is served as the app
when GARDEN_UI_DIR is unset; without one, Sophia serves its API and MCP only.
Ctrl-C stops it after a final flush.
";

const URL_HELP: &str = "\
gardend url [--profile <dir>] [--port <n>]

Prints the open link for a profile: the running gardend's port and key when
one serves the profile, else the port it would use and the profile's key.
";

const TOKEN_HELP: &str = "\
gardend token create --label <name> [--grant <grant>] [--expires-days <n>]
                     [--profile-dir <dir>] [--json]
gardend token list [--profile-dir <dir>] [--json]
gardend token revoke <id> [--profile-dir <dir>]

Client tokens for one profile, sent as \"Authorization: Bearer <token>\" to the
REST API and /mcp. `create` prints the secret once; the profile keeps only its
hash. Grants (--grant, also --profile):
  read-only    read graphs, documents, search, RDF queries (the default)
  authoring    read and write documents, workspace, artifacts, wires; no MCP tools
  member       every read, authoring's writes and MCP tools; no deletes, graph
               creation, imports or restores, RDF loads or updates. For agents:
               the grant Cloudflare Access members get
  rdf-admin    query, load, update and dump RDF
  session-all  every scope, like the master key
--expires-days: 1 to 365 (default 30). `revoke` takes the id or the first
characters `list` shows. A running gardend needs no restart: it reads the
token store on every request.
";

const SERVICE_HELP: &str = "\
gardend service install [--profile <dir>] [--port <n>] [--print]
gardend service uninstall
gardend service status

install writes and starts a user service running
`gardend serve --profile <dir> --port <n>` at login:
  macOS  ~/Library/LaunchAgents/dev.sophia.gardend.plist,
         logs in ~/Library/Logs/Sophia/gardend.log
  Linux  ~/.config/systemd/user/gardend.service,
         logs in the journal: journalctl --user -u gardend -f
It copies GARDEN_UI_DIR, GARDEN_CF_ACCESS_*, GARDEN_ALLOWED_ORIGINS,
GARDEN_PUBLIC_BASE_URL, GARDEN_LOOPBACK_HOST, GARDEN_NO_UPDATE_CHECK, RUST_LOG
and PATH from your shell into the service. --print shows the file and changes
nothing. uninstall stops and removes the service and keeps the profile.
Windows: not yet; run `gardend serve` yourself.
";

/// Flags and positionals of one verb.
#[derive(Debug, Default)]
struct Parsed {
    values: BTreeMap<String, String>,
    switches: BTreeSet<String>,
    positionals: Vec<String>,
    help: bool,
}

impl Parsed {
    fn value(&self, name: &str) -> Option<String> {
        self.values.get(name).cloned()
    }
}

fn split(args: &[String], valued: &[&str], switches: &[&str], usage: &str) -> Result<Parsed, String> {
    let mut parsed = Parsed::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "-h" || arg == "--help" {
            parsed.help = true;
            continue;
        }
        if !arg.starts_with("--") {
            parsed.positionals.push(arg.clone());
            continue;
        }
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (arg.as_str(), None),
        };
        if valued.contains(&name) {
            let value = match inline {
                Some(value) => value,
                None => iter
                    .next()
                    .cloned()
                    .ok_or_else(|| format!("{name} needs a value\n\n{usage}"))?,
            };
            parsed.values.insert(name.to_string(), value);
        } else if switches.contains(&name) && inline.is_none() {
            parsed.switches.insert(name.to_string());
        } else {
            return Err(format!("unknown option {arg}\n\n{usage}"));
        }
    }
    Ok(parsed)
}

fn no_positionals(parsed: &Parsed, usage: &str) -> Result<(), String> {
    match parsed.positionals.first() {
        Some(extra) => Err(format!("unexpected argument {extra:?}\n\n{usage}")),
        None => Ok(()),
    }
}

fn port(parsed: &Parsed, usage: &str) -> Result<Option<u16>, String> {
    parsed
        .value("--port")
        .map(|value| {
            value
                .parse::<u16>()
                .map_err(|error| format!("--port {value:?}: {error}\n\n{usage}"))
        })
        .transpose()
}

/// `--profile` or `--profile-dir`, not both.
fn profile_dir_flag(parsed: &Parsed, usage: &str) -> Result<Option<String>, String> {
    match (parsed.value("--profile"), parsed.value("--profile-dir")) {
        (Some(_), Some(_)) => Err(format!("give --profile or --profile-dir, not both\n\n{usage}")),
        (one, other) => Ok(one.or(other)),
    }
}

fn serve_args(rest: &[String], usage: &str) -> Result<Result<ServeArgs, ()>, String> {
    let parsed = split(rest, &["--profile", "--profile-dir", "--port"], &[], usage)?;
    if parsed.help {
        return Ok(Err(()));
    }
    no_positionals(&parsed, usage)?;
    Ok(Ok(ServeArgs {
        profile: profile_dir_flag(&parsed, usage)?,
        port: port(&parsed, usage)?,
    }))
}

/// `Ok(None)`: not a self-host verb (gardend carries on as before).
/// `Err`: a self-host verb used wrongly; the text includes its usage.
pub fn parse(args: &[String]) -> Result<Option<Command>, String> {
    let Some(verb) = args.first().map(String::as_str) else {
        return Ok(None);
    };
    let rest = &args[1..];
    let help = |topic: &str| -> Result<Option<Command>, String> {
        Ok(Some(Command::Help(Some(topic.to_string()))))
    };
    match verb {
        "help" | "--help" | "-h" => Ok(Some(Command::Help(rest.first().cloned()))),
        "serve" => match serve_args(rest, SERVE_HELP)? {
            Ok(args) => Ok(Some(Command::Serve(args))),
            Err(()) => help("serve"),
        },
        "url" => match serve_args(rest, URL_HELP)? {
            Ok(args) => Ok(Some(Command::Url(args))),
            Err(()) => help("url"),
        },
        "token" => parse_token(rest),
        "service" => parse_service(rest),
        _ => Ok(None),
    }
}

fn parse_token(rest: &[String]) -> Result<Option<Command>, String> {
    let help: Result<Option<Command>, String> =
        Ok(Some(Command::Help(Some("token".to_string()))));
    let Some(action) = rest.first().map(String::as_str) else {
        return Err(format!("which token action?\n\n{TOKEN_HELP}"));
    };
    let rest = &rest[1..];
    match action {
        "-h" | "--help" | "help" => help,
        "create" => {
            let parsed = split(
                rest,
                &["--label", "--profile", "--grant", "--expires-days", "--profile-dir"],
                &["--json"],
                TOKEN_HELP,
            )?;
            if parsed.help {
                return help;
            }
            no_positionals(&parsed, TOKEN_HELP)?;
            let label = parsed
                .value("--label")
                .map(|label| label.trim().to_string())
                .filter(|label| !label.is_empty())
                .ok_or_else(|| {
                    format!("token create needs --label <name>, so you can tell tokens apart\n\n{TOKEN_HELP}")
                })?;
            let grant = match (parsed.value("--profile"), parsed.value("--grant")) {
                (Some(_), Some(_)) => {
                    return Err(format!(
                        "give --grant or --profile (the same thing), not both\n\n{TOKEN_HELP}"
                    ))
                }
                (one, other) => one.or(other).unwrap_or_else(|| "read-only".to_string()),
            };
            if !GRANT_PROFILES.contains(&grant.as_str()) {
                return Err(format!(
                    "unknown grant {grant:?}: choose one of {}\n\n{TOKEN_HELP}",
                    GRANT_PROFILES.join(", ")
                ));
            }
            let expires_days = parsed
                .value("--expires-days")
                .map(|value| {
                    value
                        .parse::<u32>()
                        .map_err(|error| format!("--expires-days {value:?}: {error}\n\n{TOKEN_HELP}"))
                })
                .transpose()?;
            Ok(Some(Command::TokenCreate(TokenCreateArgs {
                label,
                grant,
                expires_days,
                profile_dir: parsed.value("--profile-dir"),
                json: parsed.switches.contains("--json"),
            })))
        }
        "list" => {
            let parsed = split(rest, &["--profile-dir"], &["--json"], TOKEN_HELP)?;
            if parsed.help {
                return help;
            }
            no_positionals(&parsed, TOKEN_HELP)?;
            Ok(Some(Command::TokenList {
                profile_dir: parsed.value("--profile-dir"),
                json: parsed.switches.contains("--json"),
            }))
        }
        "revoke" => {
            let parsed = split(rest, &["--profile-dir"], &[], TOKEN_HELP)?;
            if parsed.help {
                return help;
            }
            let [id] = parsed.positionals.as_slice() else {
                return Err(format!("token revoke takes one token id\n\n{TOKEN_HELP}"));
            };
            Ok(Some(Command::TokenRevoke {
                id: id.clone(),
                profile_dir: parsed.value("--profile-dir"),
            }))
        }
        other => Err(format!("unknown token action {other:?}\n\n{TOKEN_HELP}")),
    }
}

fn parse_service(rest: &[String]) -> Result<Option<Command>, String> {
    let help: Result<Option<Command>, String> =
        Ok(Some(Command::Help(Some("service".to_string()))));
    let Some(action) = rest.first().map(String::as_str) else {
        return Err(format!("which service action?\n\n{SERVICE_HELP}"));
    };
    let rest = &rest[1..];
    match action {
        "-h" | "--help" | "help" => help,
        "install" => {
            let parsed = split(
                rest,
                &["--profile", "--profile-dir", "--port"],
                &["--print"],
                SERVICE_HELP,
            )?;
            if parsed.help {
                return help;
            }
            no_positionals(&parsed, SERVICE_HELP)?;
            Ok(Some(Command::ServiceInstall(ServiceArgs {
                profile: profile_dir_flag(&parsed, SERVICE_HELP)?,
                port: port(&parsed, SERVICE_HELP)?,
                print: parsed.switches.contains("--print"),
            })))
        }
        "uninstall" | "status" => {
            let parsed = split(rest, &[], &[], SERVICE_HELP)?;
            if parsed.help {
                return help;
            }
            no_positionals(&parsed, SERVICE_HELP)?;
            Ok(Some(if action == "status" {
                Command::ServiceStatus
            } else {
                Command::ServiceUninstall
            }))
        }
        other => Err(format!("unknown service action {other:?}\n\n{SERVICE_HELP}")),
    }
}

pub(crate) fn help_text(topic: Option<&str>) -> &'static str {
    match topic {
        Some("serve") => SERVE_HELP,
        Some("url") => URL_HELP,
        Some("token") => TOKEN_HELP,
        Some("service") => SERVICE_HELP,
        _ => HELP,
    }
}

/// Run every verb except `serve` (which boots the engine in gardend's own
/// `main`). Returns the process exit code.
pub fn run(command: Command) -> i32 {
    let env: Env = &process_env;
    let os = std::env::consts::OS;
    let outcome: Result<(String, i32), String> = match command {
        Command::Help(topic) => Ok((help_text(topic.as_deref()).to_string(), 0)),
        Command::Serve(_) => Err("serve boots the engine from gardend's main".to_string()),
        Command::Url(args) => url(&args, os, env).map(|text| (text, 0)),
        Command::TokenCreate(args) => resolve_profile_dir(args.profile_dir.as_deref(), os, env)
            .and_then(|dir| tokens::create(&dir, &args))
            .map(|text| (text, 0)),
        Command::TokenList { profile_dir, json } => {
            resolve_profile_dir(profile_dir.as_deref(), os, env)
                .and_then(|dir| tokens::list(&dir, json))
                .map(|text| (text, 0))
        }
        Command::TokenRevoke { id, profile_dir } => {
            resolve_profile_dir(profile_dir.as_deref(), os, env)
                .and_then(|dir| tokens::revoke(&dir, &id))
                .map(|text| (text, 0))
        }
        Command::ServiceInstall(args) => service::install(&args, env).map(|text| (text, 0)),
        Command::ServiceUninstall => service::uninstall(env).map(|text| (text, 0)),
        Command::ServiceStatus => {
            service::status(env).map(|(text, running)| (text, if running { 0 } else { 3 }))
        }
    };
    match outcome {
        Ok((text, code)) => {
            print!("{text}");
            code
        }
        Err(error) => {
            eprintln!("gardend: {error}");
            if error.contains("not available on Windows") {
                2
            } else {
                1
            }
        }
    }
}

/// `gardend url`: the link of the gardend serving the profile, else the
/// link `gardend serve` will print for it.
fn url(args: &ServeArgs, os: &str, env: Env) -> Result<String, String> {
    let profile_dir = resolve_profile_dir(args.profile.as_deref(), os, env)?;
    if let Some(running) = serving_now(&profile_dir) {
        if !running.token.is_empty() {
            return Ok(format!(
                "{}\n",
                open_url("127.0.0.1", running.port, &running.token)
            ));
        }
    }
    ensure_profile_dir(&profile_dir)?;
    let (key, _) = ensure_profile_key(&profile_dir)?;
    let port = resolve_port(args.port, env)?;
    eprintln!(
        "(no gardend is serving {} right now; start it with `gardend serve`)",
        profile_dir.display()
    );
    Ok(format!("{}\n", open_url("127.0.0.1", port, &key)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| word.to_string()).collect()
    }

    #[test]
    fn plain_gardend_and_old_commands_are_not_self_host_verbs() {
        assert_eq!(parse(&argv(&[])).unwrap(), None);
        assert_eq!(parse(&argv(&["--version"])).unwrap(), None);
        assert_eq!(parse(&argv(&["--check-update"])).unwrap(), None);
        assert_eq!(parse(&argv(&["update", "--check"])).unwrap(), None);
        assert_eq!(parse(&argv(&["something-else"])).unwrap(), None);
    }

    #[test]
    fn serve_takes_profile_and_port_in_either_spelling() {
        assert_eq!(
            parse(&argv(&["serve"])).unwrap(),
            Some(Command::Serve(ServeArgs::default()))
        );
        let expected = Some(Command::Serve(ServeArgs {
            profile: Some("/data/sophia".to_string()),
            port: Some(7700),
        }));
        assert_eq!(parse(&argv(&["serve", "--profile", "/data/sophia", "--port", "7700"])).unwrap(), expected);
        assert_eq!(parse(&argv(&["serve", "--profile=/data/sophia", "--port=7700"])).unwrap(), expected);
        assert_eq!(parse(&argv(&["serve", "--profile-dir", "/data/sophia", "--port", "7700"])).unwrap(), expected);
        assert!(parse(&argv(&["serve", "--port", "99999"])).unwrap_err().contains("--port"));
        assert!(parse(&argv(&["serve", "--port"])).unwrap_err().contains("needs a value"));
        assert!(parse(&argv(&["serve", "--bogus"])).unwrap_err().contains("unknown option --bogus"));
        assert!(parse(&argv(&["serve", "extra"])).unwrap_err().contains("unexpected argument"));
        assert!(parse(&argv(&["serve", "--profile", "a", "--profile-dir", "b"])).is_err());
        assert_eq!(
            parse(&argv(&["serve", "--help"])).unwrap(),
            Some(Command::Help(Some("serve".to_string())))
        );
    }

    #[test]
    fn token_create_reads_label_grant_expiry_and_profile_dir() {
        assert_eq!(
            parse(&argv(&[
                "token", "create", "--label", "lucy-phone", "--profile", "authoring",
                "--expires-days", "365", "--profile-dir", "/data/sophia", "--json",
            ]))
            .unwrap(),
            Some(Command::TokenCreate(TokenCreateArgs {
                label: "lucy-phone".to_string(),
                grant: "authoring".to_string(),
                expires_days: Some(365),
                profile_dir: Some("/data/sophia".to_string()),
                json: true,
            }))
        );
        let Some(Command::TokenCreate(defaults)) =
            parse(&argv(&["token", "create", "--label", "agent"])).unwrap()
        else {
            panic!("token create");
        };
        assert_eq!(defaults.grant, "read-only");
        assert_eq!(defaults.expires_days, None);
        let Some(Command::TokenCreate(by_alias)) =
            parse(&argv(&["token", "create", "--label", "a", "--grant", "rdf-admin"])).unwrap()
        else {
            panic!("token create --grant");
        };
        assert_eq!(by_alias.grant, "rdf-admin");
        let Some(Command::TokenCreate(member)) =
            parse(&argv(&["token", "create", "--label", "claude", "--grant", "member"])).unwrap()
        else {
            panic!("token create --grant member");
        };
        assert_eq!(member.grant, "member");
        let Some(Command::TokenCreate(member)) =
            parse(&argv(&["token", "create", "--label", "claude", "--profile=member"])).unwrap()
        else {
            panic!("token create --profile member");
        };
        assert_eq!(member.grant, "member", "--profile stays an alias of --grant");
        assert!(parse(&argv(&["token", "create", "--label", "a", "--grant", "member", "--profile", "member"]))
            .unwrap_err()
            .contains("not both"));
        assert!(parse(&argv(&["token", "create"])).unwrap_err().contains("--label"));
        assert!(parse(&argv(&["token", "create", "--label", "a", "--profile", "root"]))
            .unwrap_err()
            .contains("unknown grant"));
        assert!(parse(&argv(&["token", "create", "--label", "a", "--expires-days", "soon"])).is_err());
        assert!(parse(&argv(&["token", "create", "--label", "a", "--json=yes"])).is_err());
    }

    #[test]
    fn token_list_and_revoke() {
        assert_eq!(
            parse(&argv(&["token", "list", "--json"])).unwrap(),
            Some(Command::TokenList { profile_dir: None, json: true })
        );
        assert_eq!(
            parse(&argv(&["token", "revoke", "3f2a9c1e", "--profile-dir", "/p"])).unwrap(),
            Some(Command::TokenRevoke {
                id: "3f2a9c1e".to_string(),
                profile_dir: Some("/p".to_string())
            })
        );
        assert!(parse(&argv(&["token", "revoke"])).is_err());
        assert!(parse(&argv(&["token", "revoke", "a", "b"])).is_err());
        assert!(parse(&argv(&["token"])).is_err());
        assert!(parse(&argv(&["token", "mint"])).is_err());
    }

    #[test]
    fn service_verbs() {
        assert_eq!(
            parse(&argv(&["service", "install", "--profile", "/p", "--port", "7701", "--print"])).unwrap(),
            Some(Command::ServiceInstall(ServiceArgs {
                profile: Some("/p".to_string()),
                port: Some(7701),
                print: true,
            }))
        );
        assert_eq!(parse(&argv(&["service", "status"])).unwrap(), Some(Command::ServiceStatus));
        assert_eq!(parse(&argv(&["service", "uninstall"])).unwrap(), Some(Command::ServiceUninstall));
        assert!(parse(&argv(&["service", "restart"])).is_err());
        assert!(parse(&argv(&["service"])).is_err());
    }

    #[test]
    fn help_names_every_verb_and_the_default_port() {
        assert_eq!(parse(&argv(&["--help"])).unwrap(), Some(Command::Help(None)));
        assert_eq!(
            parse(&argv(&["help", "token"])).unwrap(),
            Some(Command::Help(Some("token".to_string())))
        );
        let help = help_text(None);
        for verb in ["serve", "url", "token", "service"] {
            assert!(help.contains(&format!("gardend {verb}")), "{verb}");
        }
        assert!(help.contains(&super::super::DEFAULT_PORT.to_string()));
        assert!(help_text(Some("serve")).contains("Open Sophia: http://127.0.0.1:<port>/#key=<key>"));
        for grant in GRANT_PROFILES {
            assert!(help_text(Some("token")).contains(grant), "{grant}");
        }
        assert!(help.contains("--grant member"), "the overview tells agents which grant to use");
    }
}
