//! W6: a signed-in visitor writes in one folder of a public Sophia, and only
//! there (hub design `plans/2026-10-14-cow-tools-demo/folder-scoped-edit.md`).
//!
//! Public mode (`public_mode`) makes every caller without a credential, and
//! every Sophia account, a viewer of the listed graphs. A `visitorFolders`
//! entry in `public.json` opens one folder subtree of one public graph to
//! signed-in accounts with a verified email: there an account creates
//! documents and edits, renames, moves (inside the subtree), wires and trashes
//! **its own** documents. Everything else stays read-only.
//!
//! How it holds (fail closed by construction):
//!
//! - **No write scope.** An account keeps `PUBLIC_SCOPES` (reads only). A
//!   write is admitted only where this module is asked: the closed tool table
//!   [`FENCED_TOOLS`] at `/mcp`, two REST routes (trash, restore), and the
//!   document rooms. Any other path sees a read-only caller and refuses.
//! - **The fence travels with the write.** An admitted request runs inside
//!   [`CURRENT_FENCE`]; the CRDT enqueue stamps the fence into the journaled
//!   payload ([`VISITOR_FENCE_PAYLOAD_KEY`], stripped from every caller's
//!   payload first), refuses any kind outside [`FENCED_KINDS`], and the
//!   executor checks ownership and place again ([`admit_operation`]) when the
//!   operation runs (and when a recovered one replays).
//! - **Ownership** is `createdBy` on the workspace entry: written by the
//!   server only, in a fenced `workspace.createDocument`, as the account's
//!   opaque id (`acct-…`), never its Cognito subject or its email.
//! - **Rooms:** a visitor never writes the workspace Y.Doc (one raw update can
//!   rewrite the tree); a document room takes a visitor's frames only for its
//!   own documents inside the fence, each frame shadow-applied and checked
//!   against the content policy ([`check_document_policy`]) before it lands.
//! - **The kill switch** `<profile>/public/visitor-writes.off` (`gardend
//!   visitors off`) turns every fence off at the next request or frame.
//!
//! Off unless configured: no `visitorFolders`, no fence, nothing here runs. A
//! hosted cell never loads public mode, so never a fence either.

use crate::app_runtime::AppHandle;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{Mutex, PoisonError},
    time::{Duration, Instant, SystemTime},
};
#[cfg(feature = "desktop")]
use tauri::Manager;
use yrs::{types::ToJson, Any, Map as _, MapRef, Out, ReadTxn};

/// Server-only payload key: the fence an operation was admitted under.
/// Stripped from every caller's payload at enqueue; journaled.
pub(crate) const VISITOR_FENCE_PAYLOAD_KEY: &str = "__gardenVisitorFence";
/// The visitors' state, beside `public.json`: `<profile>/public/`.
pub(crate) const VISITOR_STATE_DIR: &str = "public";
/// Present: no visitor writes anywhere (`gardend visitors off`).
pub(crate) const KILL_SWITCH_FILE: &str = "visitor-writes.off";
/// `{"banned": ["acct-…" | "cognito:<sub>", …]}` (`gardend visitors ban`).
pub(crate) const BANS_FILE: &str = "visitor-bans.json";
/// The accounts gardend has seen write, their names and colours.
pub(crate) const ACCOUNTS_FILE: &str = "visitors.json";
const SALT_FILE: &str = "visitor-salt";

/// The workspace entry field that says whose a document is (server-only).
pub(crate) const CREATED_BY_FIELD: &str = "createdBy";
/// The name the account had when it made the document (display only).
pub(crate) const CREATED_BY_NAME_FIELD: &str = "createdByName";

/// The MCP tools a fence may admit (each with its argument check in
/// [`admit_tool`]). Every other tool keeps the refusal a viewer gets.
pub(crate) const FENCED_TOOLS: &[&str] = &[
    "create_document",
    "write_document",
    "insert_blocks",
    "update_blocks",
    "edit_block_text",
    "delete_blocks",
    "edit_comment",
    "rename",
    "move_documents",
    "create_wires",
    "delete",
];

/// The CRDT operations a fenced request may enqueue (its admitted tools and
/// routes, and their own follow-ups). Every other kind is refused at enqueue
/// and again in the executor.
pub(crate) const FENCED_KINDS: &[&str] = &[
    "crdt.flush",
    "workspace.createDocument",
    "workspace.updateDocument",
    "workspace.moveDocuments",
    "workspace.createWire",
    "workspace.deleteWire",
    "document.write",
    "document.editComment",
    "block.insert",
    "block.update",
    "block.editText",
    "block.delete",
];

/// Shrubbery's presence colours (`apps/organism/src/cell/crdt-presence.ts`
/// `PRESENCE_COLORS`), so a visitor's default colour is the one the shell
/// would give the same user id.
const PRESENCE_COLORS: [&str; 8] = [
    "#e11d48", "#ea580c", "#ca8a04", "#16a34a", "#0891b2", "#2563eb", "#7c3aed", "#c026d3",
];

const MAX_SUBTREE_DEPTH: usize = 64;
const SWITCH_CACHE: Duration = Duration::from_millis(1_000);

// ─── Configuration (`public.json` `visitorFolders`) ─────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct VisitorFolderFile {
    graph_id: String,
    folder_id: String,
    #[serde(default)]
    quotas: QuotaFile,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct QuotaFile {
    documents_per_account: Option<u32>,
    total_documents: Option<u32>,
    new_documents_per_hour: Option<u32>,
    writes_per_minute: Option<f64>,
    document_bytes: Option<usize>,
    room_bytes_per_minute: Option<usize>,
    links_per_document: Option<usize>,
    wires_per_account: Option<u32>,
}

/// The limits of one visitor folder.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Quotas {
    pub(crate) documents_per_account: u32,
    pub(crate) total_documents: u32,
    pub(crate) new_documents_per_hour: u32,
    pub(crate) writes_per_minute: f64,
    pub(crate) document_bytes: usize,
    pub(crate) room_bytes_per_minute: usize,
    pub(crate) links_per_document: usize,
    pub(crate) wires_per_account: u32,
}

impl Default for Quotas {
    fn default() -> Self {
        Self {
            documents_per_account: 20,
            total_documents: 2_000,
            new_documents_per_hour: 200,
            writes_per_minute: 30.0,
            document_bytes: 512 * 1024,
            room_bytes_per_minute: 256 * 1024,
            links_per_document: 20,
            wires_per_account: 50,
        }
    }
}

impl Eq for Quotas {}

impl PartialOrd for Quotas {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Quotas {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (
            self.documents_per_account,
            self.total_documents,
            self.new_documents_per_hour,
            self.writes_per_minute.to_bits(),
            self.document_bytes,
            self.room_bytes_per_minute,
            self.links_per_document,
            self.wires_per_account,
        )
            .cmp(&(
                other.documents_per_account,
                other.total_documents,
                other.new_documents_per_hour,
                other.writes_per_minute.to_bits(),
                other.document_bytes,
                other.room_bytes_per_minute,
                other.links_per_document,
                other.wires_per_account,
            ))
    }
}

#[derive(Debug, Clone)]
struct VisitorFolder {
    folder_id: String,
    quotas: Quotas,
}

/// The visitor folders of a public Sophia, and the state that goes with them
/// (`<profile>/public/`).
#[derive(Debug)]
pub(crate) struct VisitorFolders {
    folders: BTreeMap<String, VisitorFolder>,
    state_dir: PathBuf,
    switch: Mutex<Option<(Instant, bool)>>,
    bans: Mutex<Option<(Instant, BTreeSet<String>)>>,
    accounts: Mutex<Option<AccountsFile>>,
    salt: Mutex<Option<String>>,
    writes: Mutex<HashMap<String, (f64, Instant)>>,
    room_bytes: Mutex<HashMap<String, (f64, Instant)>>,
    /// Each account's document-room frames: typing sends one a keystroke, so
    /// not the per-minute write count, but a bound on how many copies of a
    /// document the room checks for one account (20 a second, 200 at once).
    room_frames: Mutex<HashMap<String, (f64, Instant)>>,
    creates: Mutex<VecDeque<Instant>>,
    /// Each account's changes of its bound name or colour: a few at once, then
    /// six a minute (a flood of renames writes nothing).
    presence_binds: Mutex<HashMap<String, (f64, Instant)>>,
}

impl VisitorFolders {
    /// Read `visitorFolders`: each graph must be public, one folder per graph.
    pub(crate) fn from_file(
        entries: Vec<VisitorFolderFile>,
        public_graphs: &BTreeSet<String>,
        profile_dir: &Path,
    ) -> Result<Option<Self>, String> {
        if entries.is_empty() {
            return Ok(None);
        }
        let mut folders = BTreeMap::new();
        for entry in entries {
            let graph_id = entry.graph_id.trim().to_string();
            let folder_id = entry.folder_id.trim().to_string();
            if !public_graphs.contains(&graph_id) {
                return Err(format!(
                    "public mode: visitorFolders: {graph_id:?} is not one of the public graphs"
                ));
            }
            crate::ids::validate_local_id(&folder_id, "visitor folder")
                .map_err(|error| format!("public mode: visitorFolders: {error}"))?;
            let defaults = Quotas::default();
            let q = entry.quotas;
            let quotas = Quotas {
                documents_per_account: q
                    .documents_per_account
                    .unwrap_or(defaults.documents_per_account),
                total_documents: q.total_documents.unwrap_or(defaults.total_documents),
                new_documents_per_hour: q
                    .new_documents_per_hour
                    .unwrap_or(defaults.new_documents_per_hour),
                writes_per_minute: q.writes_per_minute.unwrap_or(defaults.writes_per_minute),
                document_bytes: q.document_bytes.unwrap_or(defaults.document_bytes),
                room_bytes_per_minute: q
                    .room_bytes_per_minute
                    .unwrap_or(defaults.room_bytes_per_minute),
                links_per_document: q.links_per_document.unwrap_or(defaults.links_per_document),
                wires_per_account: q.wires_per_account.unwrap_or(defaults.wires_per_account),
            };
            if !(quotas.writes_per_minute.is_finite() && quotas.writes_per_minute > 0.0) {
                return Err(
                    "public mode: visitorFolders quotas.writesPerMinute must be above 0".into(),
                );
            }
            if folders
                .insert(graph_id.clone(), VisitorFolder { folder_id, quotas })
                .is_some()
            {
                return Err(format!(
                    "public mode: visitorFolders: one folder per graph ({graph_id:?} twice)"
                ));
            }
        }
        Ok(Some(Self {
            folders,
            state_dir: profile_dir.join(VISITOR_STATE_DIR),
            switch: Mutex::new(None),
            bans: Mutex::new(None),
            accounts: Mutex::new(None),
            salt: Mutex::new(None),
            writes: Mutex::new(HashMap::new()),
            room_bytes: Mutex::new(HashMap::new()),
            room_frames: Mutex::new(HashMap::new()),
            creates: Mutex::new(VecDeque::new()),
            presence_binds: Mutex::new(HashMap::new()),
        }))
    }

    /// The `visitorFolders` section of `/self-host.json`.
    pub(crate) fn describe(&self) -> Value {
        Value::Array(
            self.folders
                .iter()
                .map(|(graph, folder)| {
                    json!({
                        "graphId": graph,
                        "folderId": folder.folder_id,
                        "open": !self.writes_off(),
                        "requires": "a Sophia account with a verified email",
                    })
                })
                .collect(),
        )
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// True when the kill switch file is there (read at most once a second).
    pub(crate) fn writes_off(&self) -> bool {
        let mut cached = self.switch.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((at, off)) = *cached {
            if at.elapsed() < SWITCH_CACHE {
                return off;
            }
        }
        let off = self.state_dir.join(KILL_SWITCH_FILE).exists();
        *cached = Some((Instant::now(), off));
        off
    }

    /// Forget the cached switch (tests, and the CLI flipping it in-process).
    pub(crate) fn reread_switch(&self) {
        *self.switch.lock().unwrap_or_else(PoisonError::into_inner) = None;
        *self.bans.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    fn banned(&self, account: &str, principal: &str) -> bool {
        let mut cached = self.bans.lock().unwrap_or_else(PoisonError::into_inner);
        let fresh = cached
            .as_ref()
            .is_some_and(|(at, _)| at.elapsed() < SWITCH_CACHE);
        if !fresh {
            *cached = Some((Instant::now(), read_bans(&self.state_dir)));
        }
        cached
            .as_ref()
            .is_some_and(|(_, bans)| bans.contains(account) || bans.contains(principal))
    }

    fn salt(&self) -> String {
        let mut salt = self.salt.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(salt) = salt.as_ref() {
            return salt.clone();
        }
        let path = self.state_dir.join(SALT_FILE);
        let value = match std::fs::read_to_string(&path) {
            Ok(text) if text.trim().len() >= 32 => text.trim().to_string(),
            _ => {
                let fresh = format!(
                    "{}{}",
                    uuid::Uuid::new_v4().simple(),
                    uuid::Uuid::new_v4().simple()
                );
                let _ = std::fs::create_dir_all(&self.state_dir);
                if let Err(error) = crate::storage::write_bytes(&path, fresh.as_bytes()) {
                    log::warn!("visitor fence: keep the account salt: {error}");
                }
                fresh
            }
        };
        *salt = Some(value.clone());
        value
    }

    /// The account's public id: `acct-` and 16 hex digits of a salted hash of
    /// its Cognito subject (stable for this profile; never the subject).
    pub(crate) fn account_id(&self, sub: &str) -> String {
        let digest = Sha256::digest(format!("{}\0{sub}", self.salt()).as_bytes());
        let hex: String = digest
            .iter()
            .take(8)
            .map(|byte| format!("{byte:02x}"))
            .collect();
        format!("acct-{hex}")
    }

    fn with_accounts<T>(&self, use_accounts: impl FnOnce(&mut AccountsFile) -> (T, bool)) -> T {
        let mut accounts = self.accounts.lock().unwrap_or_else(PoisonError::into_inner);
        if accounts.is_none() {
            *accounts = Some(
                crate::storage::read_json::<AccountsFile>(&self.state_dir.join(ACCOUNTS_FILE))
                    .unwrap_or_default(),
            );
        }
        let file = accounts.as_mut().expect("loaded above");
        let (value, changed) = use_accounts(file);
        if changed {
            let _ = std::fs::create_dir_all(&self.state_dir);
            if let Err(error) =
                crate::storage::write_json(&self.state_dir.join(ACCOUNTS_FILE), file)
            {
                log::warn!(
                    "visitor fence: keep the visitors' names: {}",
                    error.message()
                );
            }
        }
        value
    }

    /// The account's presence identity (name and colour), recorded at first
    /// sight with Shrubbery's defaults.
    pub(crate) fn presence_of(&self, account: &str) -> VisitorPresence {
        self.with_accounts(|file| match file.accounts.get(account) {
            Some(entry) => (
                VisitorPresence {
                    name: entry.name.clone(),
                    color: entry.color.clone(),
                },
                false,
            ),
            None => {
                let presence = default_presence(account);
                file.accounts.insert(
                    account.to_string(),
                    AccountEntry {
                        name: presence.name.clone(),
                        color: presence.color.clone(),
                        first_seen: now_iso(),
                        earlier_names: Vec::new(),
                    },
                );
                (presence, true)
            }
        })
    }

    /// Bind the name and colour the account's own presence publishes
    /// (Shrubbery's presence control). A name that fails [`valid_name`] or a
    /// colour that is not `#rrggbb` keeps the one recorded.
    pub(crate) fn bind_presence(
        &self,
        account: &str,
        name: Option<&str>,
        color: Option<&str>,
    ) -> VisitorPresence {
        let current = self.presence_of(account);
        let name = name
            .map(str::trim)
            .filter(|name| valid_name(name))
            .map(str::to_string)
            .unwrap_or(current.name.clone());
        let color = color
            .filter(|color| valid_color(color))
            .map(str::to_ascii_lowercase)
            .unwrap_or(current.color.clone());
        if name == current.name && color == current.color {
            return current;
        }
        // One name, one account: a name that reads as another account's stays
        // theirs (sealed exam D1).
        if name != current.name {
            let skeleton = name_skeleton(&name);
            let taken = self.with_accounts(|file| {
                let taken = file.accounts.iter().any(|(other, entry)| {
                    other != account
                        && std::iter::once(&entry.name)
                            .chain(entry.earlier_names.iter())
                            .any(|held| name_skeleton(held) == skeleton)
                });
                (taken, false)
            });
            if taken {
                return current;
            }
        }
        if take(&self.presence_binds, account, 1.0, 0.1, 3.0).is_err() {
            return current;
        }
        self.with_accounts(|file| {
            let entry = file
                .accounts
                .entry(account.to_string())
                .or_insert_with(|| AccountEntry {
                    name: name.clone(),
                    color: color.clone(),
                    first_seen: now_iso(),
                    earlier_names: Vec::new(),
                });
            if entry.name != name
                && !entry.earlier_names.contains(&entry.name)
                && entry.earlier_names.len() < 50
            {
                entry.earlier_names.push(entry.name.clone());
            }
            entry.name = name.clone();
            entry.color = color.clone();
            (VisitorPresence { name, color }, true)
        })
    }

    /// Take one write for `account` from its bucket, or say when to retry.
    pub(crate) fn take_write(&self, account: &str, quotas: &Quotas) -> Result<(), u64> {
        take(
            &self.writes,
            account,
            1.0,
            quotas.writes_per_minute / 60.0,
            quotas.writes_per_minute.max(5.0),
        )
    }

    /// Take `bytes` of room traffic for `account` (a minute's worth at most).
    pub(crate) fn take_room_bytes(
        &self,
        account: &str,
        bytes: usize,
        quotas: &Quotas,
    ) -> Result<(), u64> {
        take(&self.room_frames, account, 1.0, 20.0, 200.0)?;
        let per_minute = quotas.room_bytes_per_minute as f64;
        take(
            &self.room_bytes,
            account,
            bytes as f64,
            per_minute / 60.0,
            per_minute,
        )
    }

    /// Count a new visitor document against the hourly cap; crossing it
    /// trips the kill switch for everyone (and says so in the log).
    pub(crate) fn count_create(&self, quotas: &Quotas) -> Result<(), String> {
        let mut creates = self.creates.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        while creates
            .front()
            .is_some_and(|at| now.duration_since(*at) > Duration::from_secs(3_600))
        {
            creates.pop_front();
        }
        if creates.len() as u32 >= quotas.new_documents_per_hour {
            drop(creates);
            self.trip("more new visitor documents in an hour than newDocumentsPerHour");
            return Err(
                "visitor writes are paused: too many new documents this hour. The owner has been told."
                    .into(),
            );
        }
        creates.push_back(now);
        Ok(())
    }

    /// Turn visitor writes off (the kill switch), with the reason in the file.
    pub(crate) fn trip(&self, why: &str) {
        let _ = std::fs::create_dir_all(&self.state_dir);
        let path = self.state_dir.join(KILL_SWITCH_FILE);
        let body = format!("{} tripped: {why}\n", now_iso());
        if let Err(error) = crate::storage::write_bytes(&path, body.as_bytes()) {
            log::error!("visitor fence: could not trip the kill switch: {error}");
        }
        log::warn!("visitor fence: visitor writes turned off: {why}");
        self.reread_switch();
    }
}

fn take(
    table: &Mutex<HashMap<String, (f64, Instant)>>,
    key: &str,
    cost: f64,
    per_second: f64,
    burst: f64,
) -> Result<(), u64> {
    let mut table = table.lock().unwrap_or_else(PoisonError::into_inner);
    if table.len() > 20_000 {
        table.clear();
    }
    let now = Instant::now();
    let (tokens, at) = table.entry(key.to_string()).or_insert((burst, now));
    *tokens = (*tokens + now.saturating_duration_since(*at).as_secs_f64() * per_second).min(burst);
    *at = now;
    if *tokens >= cost {
        *tokens -= cost;
        Ok(())
    } else {
        Err(((cost - *tokens) / per_second.max(f64::MIN_POSITIVE))
            .ceil()
            .max(1.0) as u64)
    }
}

fn read_bans(state_dir: &Path) -> BTreeSet<String> {
    #[derive(Deserialize, Default)]
    struct Bans {
        #[serde(default)]
        banned: Vec<String>,
    }
    crate::storage::read_json::<Bans>(&state_dir.join(BANS_FILE))
        .map(|bans| {
            bans.banned
                .into_iter()
                .map(|id| id.trim().to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// Ban or unban an account (`acct-…` or `cognito:<sub>`) in a profile's
/// visitor state. Returns the list as it now is.
pub(crate) fn set_banned(state_dir: &Path, who: &str, banned: bool) -> Result<Vec<String>, String> {
    #[derive(Serialize, Deserialize, Default)]
    struct Bans {
        #[serde(default)]
        banned: Vec<String>,
    }
    let path = state_dir.join(BANS_FILE);
    let mut bans: Bans = crate::storage::read_json(&path).unwrap_or_default();
    let who = who.trim().to_string();
    bans.banned.retain(|id| id != &who);
    if banned {
        bans.banned.push(who);
    }
    bans.banned.sort();
    std::fs::create_dir_all(state_dir)
        .map_err(|error| format!("create {}: {error}", state_dir.display()))?;
    crate::storage::write_json(&path, &bans).map_err(|error| error.message())?;
    Ok(bans.banned)
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AccountsFile {
    #[serde(default)]
    pub(crate) accounts: BTreeMap<String, AccountEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AccountEntry {
    pub(crate) name: String,
    pub(crate) color: String,
    #[serde(default)]
    pub(crate) first_seen: String,
    /// Names the account gave up: its documents and comments still carry
    /// them, so no other account takes them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) earlier_names: Vec<String>,
}

/// What other people see of a visitor: the collaboration-presence identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VisitorPresence {
    pub(crate) name: String,
    pub(crate) color: String,
}

/// A visitor's document title: what the tree shows everyone, so no address in
/// it (the 96-character rule is gardend's own, `ids::normalize_title`).
pub(crate) fn title_refusal(title: &str) -> Option<&'static str> {
    let lower = title.to_lowercase();
    if lower.contains("http:")
        || lower.contains("https:")
        || lower.contains("www.")
        || lower.contains("://")
    {
        return Some("a visitor's title carries no web address");
    }
    if title.chars().any(char::is_control) {
        return Some("a visitor's title is one line of text");
    }
    None
}

/// FNV-1a over UTF-16 code units, as Shrubbery's `hash()` (presence colour).
fn presence_hash(value: &str) -> u32 {
    let mut result: u32 = 2_166_136_261;
    for unit in value.encode_utf16() {
        result ^= u32::from(unit);
        result = result.wrapping_mul(16_777_619);
    }
    result
}

fn default_presence(account: &str) -> VisitorPresence {
    let suffix: String = account
        .trim_start_matches("acct-")
        .chars()
        .take(4)
        .collect();
    VisitorPresence {
        name: format!("Visitor {suffix}"),
        color: PRESENCE_COLORS[(presence_hash(account) as usize) % PRESENCE_COLORS.len()]
            .to_string(),
    }
}

/// A visitor's name: 1 to 40 characters of Latin letters (accents too),
/// digits, single spaces and `. ' - _ ( ) &`; no address, no id, and nothing
/// that reads as a name the house gives or keeps (`Owner`, `Sophia`, `admin`,
/// `Visitor 7c73`, …), compared by [`name_skeleton`] so that invisible
/// characters, other scripts' look-alikes and `0wner` do not get past it
/// (sealed exam 2026-10-15, D1).
pub(crate) fn valid_name(name: &str) -> bool {
    let name = name.trim();
    let count = name.chars().count();
    if !(1..=40).contains(&count) || name.contains("  ") {
        return false;
    }
    let latin_letter = |ch: char| {
        ch.is_ascii_alphanumeric() || (('\u{c0}'..='\u{17f}').contains(&ch) && ch.is_alphabetic())
    };
    if !name
        .chars()
        .all(|ch| latin_letter(ch) || matches!(ch, ' ' | '.' | '\'' | '-' | '_' | '(' | ')' | '&'))
    {
        return false;
    }
    let lower = name.to_lowercase();
    if lower.contains("www.") {
        return false;
    }
    let skeleton = name_skeleton(name);
    if skeleton.is_empty() {
        return false;
    }
    // An id is not a name (a shell that knows no name publishes the user id).
    if ["acct", "anon", "cognito", "token", "service", "user"]
        .iter()
        .any(|prefix| {
            lower.starts_with(prefix)
                && lower[prefix.len()..].starts_with(|ch: char| ch == '-' || ch == '_')
        })
    {
        return false;
    }
    let exact = [
        "you",
        "localruntime",
        "localorganism",
        "house",
        "system",
        "anonymous",
        "support",
    ];
    // Anywhere in a name (round 2 of the exam: "Sophia (Owner)").
    let anywhere = [
        "owner",
        "admin",
        "moderator",
        "visitor",
        "sophia",
        "staff",
        "official",
    ];
    !exact.iter().any(|word| skeleton == name_skeleton(word))
        && !anywhere
            .iter()
            .any(|word| skeleton.contains(&name_skeleton(word)))
}

/// What a name looks like, for comparing names: lower case, letters and
/// digits only, accents dropped and look-alikes folded (`0`→`o`, `1`/`i`→`l`,
/// `rn`→`m`, …). Two names with one skeleton read as one name.
pub(crate) fn name_skeleton(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars().flat_map(char::to_lowercase) {
        let folded: &str = match ch {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' | '4' => "a",
            'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => "c",
            'ď' | 'đ' | 'ð' => "d",
            'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' | '3' => "e",
            'ĝ' | 'ğ' | 'ġ' | 'ģ' | '9' => "g",
            'ĥ' | 'ħ' => "h",
            'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' | 'i' | 'ĺ' | 'ļ' | 'ľ' | 'ŀ'
            | 'ł' | 'l' | '1' => "l",
            'ĵ' => "j",
            'ķ' | 'ĸ' => "k",
            'ñ' | 'ń' | 'ņ' | 'ň' | 'ŉ' | 'ŋ' => "n",
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' | '0' => "o",
            'ŕ' | 'ŗ' | 'ř' => "r",
            'ś' | 'ŝ' | 'ş' | 'š' | 'ſ' | '5' => "s",
            'ţ' | 'ť' | 'ŧ' | '7' => "t",
            'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => "u",
            'ŵ' => "w",
            'ý' | 'ÿ' | 'ŷ' => "y",
            'ź' | 'ż' | 'ž' | '2' => "z",
            'æ' => "ae",
            'œ' => "oe",
            'ß' => "ss",
            'þ' => "th",
            'ĳ' => "lj",
            '8' => "b",
            '6' => "b",
            ch if ch.is_ascii_alphanumeric() => {
                out.push(ch);
                continue;
            }
            _ => continue,
        };
        out.push_str(folded);
    }
    out.replace("rn", "m").replace("vv", "w").replace("cl", "d")
}

fn valid_color(color: &str) -> bool {
    color.len() == 7
        && color.starts_with('#')
        && color[1..].chars().all(|ch| ch.is_ascii_hexdigit())
}

fn now_iso() -> String {
    chrono::DateTime::<chrono::Utc>::from(SystemTime::now()).to_rfc3339()
}

// ─── The fence ───────────────────────────────────────────────────────────────

/// The account behind a public request or a delegation, as gardend verified
/// it (`LoopbackPrincipal::Account`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct VisitorAccount {
    pub(crate) sub: String,
    pub(crate) email_verified: bool,
}

impl VisitorAccount {
    pub(crate) fn of(principal: &crate::loopback_http::LoopbackPrincipal) -> Option<Self> {
        match principal {
            crate::loopback_http::LoopbackPrincipal::Account {
                sub,
                email_verified,
                ..
            } => Some(Self {
                sub: sub.clone(),
                email_verified: *email_verified,
            }),
            _ => None,
        }
    }

    pub(crate) fn principal(&self) -> String {
        format!("cognito:{}", self.sub)
    }
}

/// Where one account may write: one folder subtree of one graph.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WriteFence {
    pub(crate) graph_id: String,
    pub(crate) root_folder_id: String,
    /// `cognito:<sub>`: the audit's actor.
    pub(crate) principal: String,
    /// `acct-…`: what `createdBy` and comments carry.
    pub(crate) account: String,
    /// The account's presence name and colour when admitted.
    pub(crate) name: String,
    pub(crate) color: String,
    pub(crate) quotas: Quotas,
}

/// Why an account gets no fence (it stays a viewer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NoFence {
    NotOpen,
    NotAnAccount,
    EmailUnverified,
    Off,
    Banned,
}

impl NoFence {
    pub(crate) fn message(&self, graph_id: &str) -> String {
        match self {
            Self::NotOpen => {
                format!("visitors cannot write in {graph_id}: this Sophia is open to read")
            }
            Self::NotAnAccount => {
                "sign in with a Sophia account to write in the visitors' folder".into()
            }
            Self::EmailUnverified => {
                "verify your account's email to write in the visitors' folder".into()
            }
            Self::Off => "visitor writing is paused right now; reading goes on".into(),
            Self::Banned => "this account may no longer write here".into(),
        }
    }

    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::NotOpen | Self::NotAnAccount => "public_read_only",
            Self::EmailUnverified => "email_unverified",
            Self::Off => "visitor_writes_paused",
            Self::Banned => "visitor_banned",
        }
    }
}

/// The fence `account` has in `graph_id` on this Sophia, or why it has none.
pub(crate) fn fence_for(
    public: Option<&crate::public_mode::PublicMode>,
    account: Option<&VisitorAccount>,
    graph_id: &str,
) -> Result<WriteFence, NoFence> {
    let folders = public
        .and_then(crate::public_mode::PublicMode::visitor_folders)
        .ok_or(NoFence::NotOpen)?;
    let folder = folders
        .folders
        .get(graph_id.trim())
        .ok_or(NoFence::NotOpen)?;
    let account = account.ok_or(NoFence::NotAnAccount)?;
    if !account.email_verified {
        return Err(NoFence::EmailUnverified);
    }
    if folders.writes_off() {
        return Err(NoFence::Off);
    }
    let id = folders.account_id(&account.sub);
    let principal = account.principal();
    if folders.banned(&id, &principal) {
        return Err(NoFence::Banned);
    }
    let presence = folders.presence_of(&id);
    Ok(WriteFence {
        graph_id: graph_id.trim().to_string(),
        root_folder_id: folder.folder_id.clone(),
        principal,
        account: id,
        name: presence.name,
        color: presence.color,
        quotas: folder.quotas,
    })
}

/// A fenced request's context: the fence, and the scopes it lends the one
/// admitted route (REST trash and restore; MCP passes them directly).
#[derive(Debug, Clone)]
pub(crate) struct FenceContext {
    pub(crate) fence: WriteFence,
    pub(crate) lent_scopes: Vec<String>,
}

tokio::task_local! {
    static CURRENT_FENCE: FenceContext;
    static ROOM_POLICY: RoomPolicy;
}

/// The fence the request this task serves was admitted under.
pub(crate) fn current_fence() -> Option<WriteFence> {
    CURRENT_FENCE.try_with(|context| context.fence.clone()).ok()
}

/// The scopes an admitted fenced route lends its caller for this request.
pub(crate) fn current_lent_scopes(principal: &str) -> Vec<String> {
    CURRENT_FENCE
        .try_with(|context| {
            if context.fence.principal == principal {
                context.lent_scopes.clone()
            } else {
                Vec::new()
            }
        })
        .unwrap_or_default()
}

/// Run `future` as a write admitted under `fence`.
pub(crate) async fn writing_within<F: std::future::Future>(
    fence: WriteFence,
    lent_scopes: Vec<String>,
    future: F,
) -> F::Output {
    CURRENT_FENCE
        .scope(FenceContext { fence, lent_scopes }, future)
        .await
}

/// The fence an operation was admitted under (from its journaled payload).
pub(crate) fn fence_of(payload: &Value) -> Option<WriteFence> {
    payload
        .get(VISITOR_FENCE_PAYLOAD_KEY)
        .and_then(|value| serde_json::from_value(value.clone()).ok())
}

// ─── At enqueue ──────────────────────────────────────────────────────────────

/// At enqueue: drop any caller copy of the server-only key, then, in a fenced
/// request, refuse a kind the fence does not admit and stamp the fence.
pub(crate) fn stamp_at_enqueue(
    kind: &str,
    graph_id: &str,
    payload: &mut Value,
) -> Result<(), String> {
    if let Some(object) = payload.as_object_mut() {
        object.remove(VISITOR_FENCE_PAYLOAD_KEY);
    }
    let Some(fence) = current_fence() else {
        return Ok(());
    };
    if !FENCED_KINDS.contains(&kind) {
        return Err(format!(
            "a visitor cannot {kind}: visitors write only their own documents in the visitors' folder"
        ));
    }
    if graph_id.trim() != fence.graph_id {
        return Err(format!(
            "a visitor writes only in {}; this names another graph",
            fence.graph_id
        ));
    }
    let object = payload
        .as_object_mut()
        .ok_or_else(|| format!("{kind}: the payload must be an object"))?;
    if kind == "workspace.createDocument" {
        let named = ["documentId", "document_id", "id"]
            .iter()
            .any(|key| object.get(*key).is_some_and(|value| !value.is_null()));
        if named {
            return Err(
                "a visitor's new document gets its id from the server: omit documentId".into(),
            );
        }
        if object
            .get("ifExists")
            .or_else(|| object.get("if_exists"))
            .is_some_and(|mode| !mode.is_null() && mode.as_str() != Some("refuse"))
        {
            return Err("a visitor's create never takes over a document: omit ifExists".into());
        }
    }
    object.insert(
        VISITOR_FENCE_PAYLOAD_KEY.to_string(),
        serde_json::to_value(&fence).map_err(|error| error.to_string())?,
    );
    Ok(())
}

// ─── The workspace, as a fence reads it ─────────────────────────────────────

fn row_text<T: ReadTxn>(txn: &T, map: &MapRef, key: &str) -> Option<String> {
    match map.get(txn, key) {
        Some(Out::Any(Any::String(text))) => Some(text.to_string()),
        _ => None,
    }
}

fn row_flag<T: ReadTxn>(txn: &T, map: &MapRef, key: &str) -> bool {
    matches!(map.get(txn, key), Some(Out::Any(Any::Bool(true))))
}

/// A workspace entry as the fence reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DocumentPlace {
    pub(crate) parent_id: Option<String>,
    pub(crate) created_by: Option<String>,
    pub(crate) trashed: bool,
}

/// The document's place and owner, `None` when the workspace has no such row.
pub(crate) fn document_place<T: ReadTxn>(txn: &T, document_id: &str) -> Option<DocumentPlace> {
    let documents = txn.get_map("documents")?;
    match documents.get(txn, document_id)? {
        Out::YMap(row) => Some(DocumentPlace {
            parent_id: row_text(txn, &row, "parentId"),
            created_by: row_text(txn, &row, CREATED_BY_FIELD),
            trashed: row_flag(txn, &row, "trashed"),
        }),
        _ => None,
    }
}

/// True when `parent_id` is the fence's folder or a folder below it.
pub(crate) fn in_subtree<T: ReadTxn>(
    txn: &T,
    root_folder_id: &str,
    parent_id: Option<&str>,
) -> bool {
    let Some(folders) = txn.get_map("folders") else {
        return false;
    };
    if !matches!(folders.get(txn, root_folder_id), Some(Out::YMap(_))) {
        return false;
    }
    let mut current = parent_id.map(str::to_string);
    for _ in 0..MAX_SUBTREE_DEPTH {
        let Some(folder_id) = current else {
            return false;
        };
        if folder_id == root_folder_id {
            return true;
        }
        current = match folders.get(txn, &folder_id) {
            Some(Out::YMap(folder)) => row_text(txn, &folder, "parentId"),
            _ => return false,
        };
    }
    false
}

/// The account's own, live document inside the fence: else why not.
pub(crate) fn own_document<T: ReadTxn>(
    txn: &T,
    fence: &WriteFence,
    document_id: &str,
) -> Result<DocumentPlace, String> {
    let place = document_place(txn, document_id)
        .ok_or_else(|| format!("no document {document_id:?} in {}", fence.graph_id))?;
    if place.created_by.as_deref() != Some(fence.account.as_str()) {
        return Err(format!(
            "{document_id:?} is not yours: a visitor edits only the documents they made"
        ));
    }
    if !in_subtree(txn, &fence.root_folder_id, place.parent_id.as_deref()) {
        return Err(format!(
            "{document_id:?} is no longer in the visitors' folder, so it is read-only"
        ));
    }
    Ok(place)
}

fn count_documents<T: ReadTxn>(txn: &T, account: Option<&str>) -> u32 {
    let Some(documents) = txn.get_map("documents") else {
        return 0;
    };
    documents
        .iter(txn)
        .filter(|(_, row)| match row {
            Out::YMap(row) => match (row_text(txn, row, CREATED_BY_FIELD), account) {
                (Some(by), Some(account)) => by == account,
                (Some(_), None) => true,
                _ => false,
            },
            _ => false,
        })
        .count() as u32
}

fn count_wires<T: ReadTxn>(txn: &T, account: &str) -> u32 {
    let (Some(wires), Some(_)) = (txn.get_map("wires"), txn.get_map("documents")) else {
        return 0;
    };
    wires
        .iter(txn)
        .filter(|(_, wire)| match wire {
            Out::YMap(wire) => row_text(txn, wire, "sourceDocumentId")
                .and_then(|source| document_place(txn, &source))
                .and_then(|place| place.created_by)
                .is_some_and(|by| by == account),
            _ => false,
        })
        .count() as u32
}

fn payload_text(payload: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| payload.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn payload_names(payload: &Map<String, Value>, keys: &[&str]) -> bool {
    keys.iter().any(|key| payload.contains_key(*key))
}

/// Is a wire predicate one of gardend's own (`wire_predicates`), short or as
/// gardend writes it (`http://mnemosyne.ai/vocab#<name>`)?
pub(crate) fn builtin_predicate(predicate: Option<&str>) -> bool {
    let Some(predicate) = predicate.map(str::trim).filter(|p| !p.is_empty()) else {
        return true; // gardend's default, isWiredTo
    };
    let short = crate::wire_predicates::predicate_short_name(predicate);
    let known = crate::wire_predicates::builtin_wire_predicates()
        .into_iter()
        .any(|(name, _, _)| name == short);
    known && (predicate == short || predicate == format!("http://mnemosyne.ai/vocab#{short}"))
}

/// Check one admitted operation against the workspace as it is now: the
/// executor runs this before the operation, and for a recovered one again.
/// Operations run one at a time, and a visitor never writes the workspace
/// room, so what this reads is what the operation meets.
pub(crate) fn check_operation<T: ReadTxn>(
    txn: &T,
    fence: &WriteFence,
    kind: &str,
    document_id: Option<&str>,
    payload: &Map<String, Value>,
) -> Result<(), String> {
    let target = || {
        document_id
            .map(str::to_string)
            .or_else(|| payload_text(payload, &["documentId", "document_id"]))
            .ok_or_else(|| format!("{kind}: name the document"))
    };
    let parent_inside = |key_sets: &[&str]| -> Result<(), String> {
        if payload_names(payload, key_sets) {
            let parent = payload_text(payload, key_sets);
            if !in_subtree(txn, &fence.root_folder_id, parent.as_deref()) {
                return Err(
                    "a visitor's documents stay in the visitors' folder: that place is outside it"
                        .into(),
                );
            }
        }
        Ok(())
    };
    if let Some(title) = payload_text(payload, &["title"]) {
        if let Some(why) = title_refusal(&title) {
            return Err(why.into());
        }
    }
    match kind {
        "crdt.flush" => Ok(()),
        "workspace.createDocument" => {
            let parent = payload_text(payload, &["parentId", "parent_id"]);
            if !in_subtree(txn, &fence.root_folder_id, parent.as_deref()) {
                return Err("a visitor creates documents only in the visitors' folder".into());
            }
            if count_documents(txn, Some(&fence.account)) >= fence.quotas.documents_per_account {
                return Err(format!(
                    "you have {} documents here, the most an account may have (documents in the Trash count too)",
                    fence.quotas.documents_per_account
                ));
            }
            if count_documents(txn, None) >= fence.quotas.total_documents {
                return Err("the visitors' folder is full for now".into());
            }
            Ok(())
        }
        "workspace.updateDocument" => {
            own_document(txn, fence, &target()?)?;
            for key in payload.keys() {
                let allowed = matches!(
                    key.as_str(),
                    "documentId" | "document_id" | "title" | "parentId" | "parent_id" | "order"
                        | "description" | "describedAt" | "described_at" | "trashed" | "updatedAt"
                        | "operationId" | "graphIncarnation" | "graph_incarnation"
                ) || key.starts_with("__garden");
                if !allowed {
                    return Err(format!("a visitor cannot change {key:?} of a document"));
                }
            }
            parent_inside(&["parentId", "parent_id"])
        }
        "workspace.moveDocuments" => {
            let ids: Vec<String> = payload
                .get("documentIds")
                .and_then(Value::as_array)
                .map(|ids| ids.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default();
            if ids.is_empty() || ids.len() > 20 {
                return Err("move 1 to 20 documents at a time".into());
            }
            for id in &ids {
                own_document(txn, fence, id)?;
            }
            let parent = payload_text(payload, &["parentId", "parent_id"]);
            if !in_subtree(txn, &fence.root_folder_id, parent.as_deref()) {
                return Err(
                    "a visitor's documents stay in the visitors' folder: that place is outside it"
                        .into(),
                );
            }
            Ok(())
        }
        "workspace.createWire" => {
            let source = document_id
                .map(str::to_string)
                .or_else(|| payload_text(payload, &["sourceDocumentId", "source_document_id"]))
                .ok_or("a wire needs its source document")?;
            // Sealed exam (2026-10-15): from a document in the Trash, no new
            // wires either (restore it first, as for its content).
            if own_document(txn, fence, &source)?.trashed {
                return Err("that document is in the Trash: restore it first".into());
            }
            if let Some(graph) = payload_text(payload, &["targetGraphId", "target_graph_id"]) {
                if graph != fence.graph_id {
                    return Err("a visitor's wires stay in this workspace".into());
                }
            }
            let target = payload_text(payload, &["targetDocumentId", "target_document_id"])
                .ok_or("a wire needs its target document")?;
            match document_place(txn, &target) {
                Some(place) if !place.trashed => {}
                _ => return Err(format!("no document {target:?} to wire to")),
            }
            // A wire id the caller names must be new: creating over an existing
            // wire would rewrite someone else's (its source, its target).
            if let (Some(id), Some(wires)) = (
                payload_text(payload, &["wireId", "wire_id", "id"]),
                txn.get_map("wires"),
            ) {
                if wires.get(txn, &id).is_some() {
                    return Err(format!("the wire {id:?} already exists: a visitor makes new wires only"));
                }
            }
            if !builtin_predicate(payload.get("predicate").and_then(Value::as_str)) {
                return Err(
                    "a visitor's wires use gardend's own predicates (list_wire_predicates)".into(),
                );
            }
            if count_wires(txn, &fence.account) >= fence.quotas.wires_per_account {
                return Err(format!(
                    "you have {} wires here, the most an account may have",
                    fence.quotas.wires_per_account
                ));
            }
            Ok(())
        }
        "workspace.deleteWire" => {
            let wire_id = payload_text(payload, &["wireId", "wire_id", "id"])
                .ok_or("name the wire")?;
            let wires = txn.get_map("wires").ok_or("no such wire")?;
            let source = match wires.get(txn, &wire_id) {
                Some(Out::YMap(wire)) => row_text(txn, &wire, "sourceDocumentId"),
                _ => return Err(format!("no wire {wire_id:?}")),
            }
            .ok_or("that wire has no source")?;
            own_document(txn, fence, &source).map(|_| ())
        }
        "document.write" => {
            let place = own_document(txn, fence, &target()?)?;
            if place.trashed {
                return Err("that document is in the Trash: restore it first".into());
            }
            for key in ["readOnly", "read_only", "ifExists"] {
                if payload.contains_key(key) {
                    return Err(format!("a visitor's write cannot carry {key:?}"));
                }
            }
            if payload
                .get("comments")
                .is_some_and(|comments| comments.as_object().is_none_or(|map| !map.is_empty()))
            {
                return Err("a visitor's write carries no comments (use edit_comment)".into());
            }
            parent_inside(&["parentId", "parent_id"])
        }
        "document.editComment" | "block.insert" | "block.update" | "block.editText" | "block.delete" => {
            let place = own_document(txn, fence, &target()?)?;
            if place.trashed {
                return Err("that document is in the Trash: restore it first".into());
            }
            Ok(())
        }
        other => Err(format!(
            "a visitor cannot {other}: visitors write only their own documents in the visitors' folder"
        )),
    }
}

/// The executor's check: the operation's fence admits it against the live
/// workspace. `Ok(None)`: not a fenced operation.
pub(crate) async fn admit_operation(
    app: &AppHandle,
    operation: &crate::crdt_operation_types::CrdtOperation,
) -> Result<Option<WriteFence>, String> {
    let Some(fence) = fence_of(&operation.payload) else {
        return Ok(None);
    };
    if !FENCED_KINDS.contains(&operation.kind.as_str()) {
        return Err(format!(
            "a visitor cannot {}: visitors write only their own documents in the visitors' folder",
            operation.kind
        ));
    }
    if operation.graph_id != fence.graph_id {
        return Err("a visitor writes only in the graph of the visitors' folder".into());
    }
    let graph_dir = crate::graph_paths::existing_graph_dir(app, &operation.graph_id)?;
    let room =
        crate::crdt_engine::workspace_ops::workspace_room(app, &operation.graph_id, &graph_dir)
            .await?;
    let payload = operation.payload.as_object().cloned().unwrap_or_default();
    let kind = operation.kind.clone();
    let document_id = operation.document_id.clone();
    let checked_fence = fence.clone();
    room.with_doc(move |doc| {
        use yrs::Transact;
        let txn = doc.transact();
        check_operation(
            &txn,
            &checked_fence,
            &kind,
            document_id.as_deref(),
            &payload,
        )
    })
    .await?;
    Ok(Some(fence))
}

// ─── The executor's document policy (`Room::update_doc`) ────────────────────

/// The words every content-policy refusal starts with: the executor turns such
/// a refusal into a terminal one (the mutation never committed), so a refused
/// visitor write fails at once instead of being retried as a hot-commit tail.
pub(crate) const POLICY_REFUSAL: &str = "visitor content policy: ";

/// While a fenced operation runs, every document-room mutation it makes is
/// checked against the content policy before it commits.
#[derive(Debug, Clone)]
pub(crate) struct RoomPolicy {
    pub(crate) fence: WriteFence,
    pub(crate) document_id: Option<String>,
}

pub(crate) fn current_room_policy() -> Option<RoomPolicy> {
    ROOM_POLICY.try_with(Clone::clone).ok()
}

pub(crate) async fn under_room_policy<F: std::future::Future>(
    policy: RoomPolicy,
    future: F,
) -> F::Output {
    ROOM_POLICY.scope(policy, future).await
}

// ─── The content policy of a visitor's document ─────────────────────────────

/// A document's parts: the text, its comments, and the server's own
/// (`metadata`; `_operationLog`, the block operations' replay ledger).
const ALLOWED_ROOTS: &[&str] = &["content", "comments", "metadata", "_operationLog"];
const ALLOWED_NODES: &[&str] = &[
    "doc",
    "paragraph",
    "heading",
    "blockquote",
    "listItem",
    "codeBlock",
    "horizontalRule",
    "hardBreak",
    "text",
    "table",
    "tableRow",
    "tableHeader",
    "tableCell",
    "footnote",
    "mathInline",
    "mathBlock",
    "tagChip",
    "wikilink",
    "bulletList",
    "orderedList",
    "taskList",
    "taskItem",
];
const ALLOWED_MARKS: &[&str] = &[
    "bold",
    "italic",
    "strike",
    "underline",
    "code",
    "highlight",
    "commentMark",
    "link",
];
const MAX_TEXT_CHARS: usize = 100_000;
const MAX_COMMENTS: usize = 200;
const MAX_COMMENT_CHARS: usize = 2_000;

fn link_allowed(href: &str) -> bool {
    let href = href.trim().to_ascii_lowercase();
    href.starts_with("https://") || href.starts_with("http://") || href.starts_with("mailto:")
}

/// Walk TipTap JSON: node types, marks, link schemes, link and text counts.
fn check_tiptap(
    node: &Value,
    graph_id: &str,
    links: &mut usize,
    chars: &mut usize,
) -> Result<(), String> {
    let kind = node.get("type").and_then(Value::as_str).unwrap_or("");
    if !ALLOWED_NODES.contains(&kind) {
        return Err(match kind {
            "image" => "images are not open to visitors yet".to_string(),
            "queryBlock" => "query blocks are not open to visitors".to_string(),
            _ => format!("a visitor's document cannot hold a {kind:?} block"),
        });
    }
    if kind == "wikilink" {
        if let Some(graph) = node
            .get("attrs")
            .and_then(|attrs| attrs.get("targetGraphId"))
            .and_then(Value::as_str)
            .filter(|graph| !graph.is_empty())
        {
            if graph != graph_id {
                return Err("a visitor's links stay in this workspace".into());
            }
        }
    }
    if let Some(text) = node.get("text").and_then(Value::as_str) {
        *chars += text.chars().count();
        if *chars > MAX_TEXT_CHARS {
            return Err(format!(
                "a visitor's document holds at most {MAX_TEXT_CHARS} characters"
            ));
        }
    }
    for mark in node
        .get("marks")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let name = mark.get("type").and_then(Value::as_str).unwrap_or("");
        if !ALLOWED_MARKS.contains(&name) {
            return Err(format!("a visitor's document cannot use the {name:?} mark"));
        }
        if name == "link" {
            *links += 1;
            let href = mark
                .get("attrs")
                .and_then(|attrs| attrs.get("href"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if !link_allowed(href) {
                return Err("a visitor's links are http, https or mailto".into());
            }
        }
    }
    for child in node
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        check_tiptap(child, graph_id, links, chars)?;
    }
    Ok(())
}

fn comments_json<T: ReadTxn>(txn: &T) -> Map<String, Value> {
    let Some(comments) = txn.get_map("comments") else {
        return Map::new();
    };
    let any = comments.to_json(txn);
    let mut text = String::new();
    any.to_json(&mut text);
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

/// The parts of a document a visitor's room frame is compared on: its
/// comments, and the server's own parts (`metadata`, `_operationLog`), which a
/// frame must leave as they are (sealed exam, round 2).
pub(crate) fn document_comments<T: ReadTxn>(txn: &T) -> Map<String, Value> {
    let mut parts = Map::new();
    parts.insert("comments".into(), Value::Object(comments_json(txn)));
    for root in SERVER_ROOTS {
        parts.insert((*root).into(), root_json(txn, root));
    }
    parts
}

/// The parts of a document only the server writes (its operations do, under
/// the fence; a visitor's own frame does not).
const SERVER_ROOTS: &[&str] = &["metadata", "_operationLog"];

fn root_json<T: ReadTxn>(txn: &T, root: &str) -> Value {
    let Some(map) = txn.get_map(root) else {
        return Value::Null;
    };
    let any = map.to_json(txn);
    let mut text = String::new();
    any.to_json(&mut text);
    let value = serde_json::from_str::<Value>(&text).unwrap_or(Value::Null);
    // An empty map reads as no map: the root's mere presence is no change.
    match &value {
        Value::Object(map) if map.is_empty() => Value::Null,
        _ => value,
    }
}

/// Check a visitor's document as it would be after a write: its roots, its
/// size, its blocks and marks, and every comment the write adds or changes
/// (`before`: the comments before it; `None` when the server signed them).
pub(crate) fn check_document_policy<T: ReadTxn>(
    txn: &T,
    fence: &WriteFence,
    encoded_bytes: usize,
    before: Option<&Map<String, Value>>,
) -> Result<(), String> {
    for (name, _) in txn.root_refs() {
        if !ALLOWED_ROOTS.contains(&name) {
            return Err(format!("a visitor's document has no {name:?} part"));
        }
    }
    if encoded_bytes > fence.quotas.document_bytes {
        return Err(format!(
            "a visitor's document is at most {} KiB",
            fence.quotas.document_bytes / 1024
        ));
    }
    let tiptap = crate::crdt_engine::projection::tiptap_json_in(txn);
    let (mut links, mut chars) = (0usize, 0usize);
    check_tiptap(&tiptap, &fence.graph_id, &mut links, &mut chars)?;
    if links > fence.quotas.links_per_document {
        return Err(format!(
            "a visitor's document holds at most {} links",
            fence.quotas.links_per_document
        ));
    }
    if let Some(parts) = before {
        for root in SERVER_ROOTS {
            if parts.get(*root).cloned().unwrap_or(Value::Null) != root_json(txn, root) {
                return Err(format!(
                    "a document's {root} is the server's: a visitor's frame leaves it as it is"
                ));
            }
        }
        let before = parts
            .get("comments")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let after = comments_json(txn);
        if after.len() > MAX_COMMENTS {
            return Err(format!("a document holds at most {MAX_COMMENTS} comments"));
        }
        for (id, comment) in &after {
            if before.get(id) == Some(comment) {
                continue;
            }
            let text = comment.get("text").and_then(Value::as_str).unwrap_or("");
            if text.chars().count() > MAX_COMMENT_CHARS {
                return Err(format!(
                    "a comment is at most {MAX_COMMENT_CHARS} characters"
                ));
            }
            // A comment written straight into the room is signed by its writer:
            // the account's id and its bound name (Shrubbery's visitor mode
            // signs it so). Unsigned, or signed by anyone else, it is refused
            // (sealed exam 2026-10-15: "You"/"local" and no author passed).
            // A comment the account signed under an earlier name keeps it.
            let author_id = comment.get("authorId").and_then(Value::as_str);
            let author = comment.get("author").and_then(Value::as_str);
            let earlier_name = before
                .get(id)
                .filter(|earlier| {
                    earlier.get("authorId").and_then(Value::as_str) == Some(fence.account.as_str())
                })
                .and_then(|earlier| earlier.get("author").and_then(Value::as_str));
            let own_id = author_id == Some(fence.account.as_str());
            let own_name =
                author.is_some() && (author == Some(fence.name.as_str()) || author == earlier_name);
            if !(own_id && own_name) {
                return Err(format!(
                    "a visitor's comments are signed as themselves (authorId {:?}, author {:?})",
                    fence.account, fence.name
                ));
            }
        }
    }
    Ok(())
}

// ─── MCP: the admitted tools and their arguments ────────────────────────────

/// Check a fenced tool call's arguments and shape them (a new document lands
/// in the visitors' folder). The executor checks ownership and place again.
pub(crate) fn admit_tool(
    name: &str,
    arguments: &mut Value,
    fence: &WriteFence,
) -> Result<(), String> {
    if !FENCED_TOOLS.contains(&name) {
        return Err(format!("{name} is not open to visitors"));
    }
    let object = arguments
        .as_object_mut()
        .ok_or_else(|| format!("{name}: arguments must be an object"))?;
    let text_len = |object: &Map<String, Value>| {
        ["content", "blocks", "tiptapJson", "text", "markdown"]
            .iter()
            .filter_map(|key| object.get(*key))
            .map(|value| match value {
                Value::String(text) => text.len(),
                other => other.to_string().len(),
            })
            .sum::<usize>()
    };
    if text_len(object) > fence.quotas.document_bytes.min(200 * 1024) {
        return Err("that is more text than a visitor's write may carry".into());
    }
    match name {
        "create_document" => {
            for key in ["documentId", "document_id", "content"] {
                if object.get(key).is_some_and(|value| !value.is_null()) {
                    return Err(match key {
                        "content" => {
                            "create_document makes an empty document; write it with write_document"
                                .into()
                        }
                        _ => {
                            "a visitor's new document gets its id from the server: omit documentId"
                                .into()
                        }
                    });
                }
            }
            if object
                .get("ifExists")
                .is_some_and(|mode| !mode.is_null() && mode.as_str() != Some("refuse"))
            {
                return Err("a visitor's create never takes over a document: omit ifExists".into());
            }
            if object.get("parentId").is_none_or(Value::is_null) {
                object.insert("parentId".into(), json!(fence.root_folder_id));
            }
        }
        "rename" => {
            let kind = ["entity_type", "entityType", "type"]
                .iter()
                .find_map(|key| object.get(*key).and_then(Value::as_str))
                .unwrap_or("")
                .to_ascii_lowercase();
            if kind != "document" {
                return Err(
                    "a visitor renames only their own documents (entity_type document)".into(),
                );
            }
        }
        "delete" => {
            let kind = object
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_ascii_lowercase();
            if !matches!(kind.as_str(), "wires" | "wire" | "blocks" | "block") {
                return Err(
                    "a visitor deletes only their own blocks and wires; move a document to the Trash instead"
                        .into(),
                );
            }
        }
        "move_documents" => {
            let count = object
                .get("documentIds")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            if count == 0 || count > 20 {
                return Err("move 1 to 20 documents at a time".into());
            }
            if object.get("parentId").is_none_or(Value::is_null) {
                return Err("name the folder (parentId) inside the visitors' folder".into());
            }
        }
        "create_wires" => {
            let count = object
                .get("wires")
                .and_then(Value::as_array)
                .map_or(1, Vec::len);
            if count > 10 {
                return Err("make at most 10 wires at a time".into());
            }
        }
        "write_document" => {
            for key in ["readOnly", "read_only", "ifExists", "parentId", "parent_id"] {
                if object.contains_key(key) {
                    return Err(format!("a visitor's write cannot carry {key:?}"));
                }
            }
            if object
                .get("comments")
                .is_some_and(|comments| comments.as_object().is_none_or(|map| !map.is_empty()))
            {
                return Err("a visitor's write carries no comments (use edit_comment)".into());
            }
        }
        _ => {}
    }
    Ok(())
}

/// The scopes an admitted tool needs (lent for this one call).
pub(crate) fn lent_scopes(name: &str) -> Vec<String> {
    crate::loopback_scopes::mcp_tool_scopes(name)
        .into_iter()
        .map(str::to_string)
        .collect()
}

// ─── Rooms: presence ─────────────────────────────────────────────────────────

/// Rewrite one awareness state a visitor publishes: the account's own id,
/// name and colour (the name and colour it chose are bound to the account
/// first, when they pass), marked as a visitor. `None` drops it.
pub(crate) fn stamp_awareness(
    folders: Option<&VisitorFolders>,
    account: Option<&str>,
    json_text: &str,
) -> Option<String> {
    if json_text == "null" {
        return Some(json_text.to_string());
    }
    let (Some(folders), Some(account)) = (folders, account) else {
        return None;
    };
    let mut state: Value = serde_json::from_str(json_text).ok()?;
    let object = state.as_object_mut()?;
    let user = object
        .get("user")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let presence = folders.bind_presence(
        account,
        user.get("name").and_then(Value::as_str),
        user.get("color").and_then(Value::as_str),
    );
    let mut stamped = user;
    stamped.insert("userId".into(), json!(account));
    stamped.insert("name".into(), json!(presence.name));
    stamped.insert("color".into(), json!(presence.color));
    stamped.insert("type".into(), json!("human"));
    stamped.insert("visitor".into(), json!(true));
    object.insert("user".into(), Value::Object(stamped));
    if let Some(lifecycle) = object.get_mut("presence").and_then(Value::as_object_mut) {
        lifecycle.insert("humanId".into(), json!(account));
    }
    serde_json::to_string(&state).ok()
}

/// The visitor folders of the running gardend (`None`: not public, or no
/// folder open, or no managed state: then nothing is admitted).
pub(crate) fn running_folders(
    app: &AppHandle,
) -> Option<std::sync::Arc<crate::loopback_state::LoopbackState>> {
    let state = app
        .try_state::<std::sync::Arc<crate::loopback_state::LoopbackState>>()?
        .inner()
        .clone();
    state.self_host.public()?.visitor_folders()?;
    Some(state)
}

/// A room frame from a fenced visitor: `Ok(true)` to apply it, `Ok(false)`
/// when it changes nothing, `Err` to refuse it (the connection closes).
pub(crate) async fn admit_room_update(
    app: &AppHandle,
    graph_id: &str,
    fence: &WriteFence,
    document_id: &str,
    room: &crate::crdt_engine::rooms::Room,
    update: &[u8],
) -> Result<bool, String> {
    let state = running_folders(app).ok_or("visitor writing is not open here")?;
    let public = state
        .self_host
        .public()
        .ok_or("visitor writing is not open here")?;
    let folders = public
        .visitor_folders()
        .ok_or("visitor writing is not open here")?;
    if folders.writes_off() {
        return Err(NoFence::Off.message(graph_id));
    }
    if folders.banned(&fence.account, &fence.principal) {
        return Err(NoFence::Banned.message(graph_id));
    }
    if graph_id != fence.graph_id {
        return Err("a visitor writes only in the graph of the visitors' folder".into());
    }
    folders
        .take_room_bytes(&fence.account, update.len().max(1), &fence.quotas)
        .map_err(|retry| format!("slow down: try again in {retry} s"))?;
    let graph_dir = crate::graph_paths::existing_graph_dir(app, graph_id)?;
    let workspace =
        crate::crdt_engine::workspace_ops::workspace_room(app, graph_id, &graph_dir).await?;
    let checked = fence.clone();
    let id = document_id.to_string();
    workspace
        .with_doc(move |doc| {
            use yrs::Transact;
            let txn = doc.transact();
            let place = own_document(&txn, &checked, &id)?;
            if place.trashed {
                return Err("that document is in the Trash: restore it first".to_string());
            }
            Ok(())
        })
        .await?;
    let policy_fence = fence.clone();
    room.shadow_apply(update, move |txn, size, before| {
        check_document_policy(txn, &policy_fence, size, Some(before))
    })
    .await
}

// ─── The Trash is the owner's to see ─────────────────────────────────────────
//
// Sealed exam (2026-10-15, C1): a document moved to the Trash (by the owner,
// moderating, or by the visitor who wrote it) stayed readable to the public by
// its id. In public mode a public request (anonymous, a signed-in account, or
// a delegation made for one) no longer reads a trashed document by id: not
// over the document routes, MCP, its room, or SPARQL. The listings already
// leave the Trash out. Its title stays in the workspace's own CRDT until the
// owner empties it from the Trash.

/// The documents of `graph_id` that are in the Trash.
pub(crate) async fn trashed_documents(
    app: &AppHandle,
    graph_id: &str,
) -> Result<BTreeSet<String>, String> {
    let graph_dir = crate::graph_paths::existing_graph_dir(app, graph_id)?;
    let room = crate::crdt_engine::workspace_ops::workspace_room(app, graph_id, &graph_dir).await?;
    Ok(room
        .with_doc(|doc| {
            use yrs::Transact;
            let txn = doc.transact();
            let mut trashed = BTreeSet::new();
            if let Some(documents) = txn.get_map("documents") {
                for (id, row) in documents.iter(&txn) {
                    if let Out::YMap(row) = row {
                        if row_flag(&txn, &row, "trashed") {
                            trashed.insert(id.to_string());
                        }
                    }
                }
            }
            trashed
        })
        .await)
}

/// The document ids a tool call's arguments name (`documentId`,
/// `document_id`, `documentIds`, a wire's source and target, at any depth).
pub(crate) fn named_document_ids(arguments: Option<&Value>) -> Vec<String> {
    fn visit(value: &Value, depth: usize, out: &mut Vec<String>) {
        if depth > 6 {
            return;
        }
        match value {
            Value::Object(map) => {
                for (key, value) in map {
                    let normalized: String = key
                        .chars()
                        .filter(|ch| *ch != '_')
                        .collect::<String>()
                        .to_ascii_lowercase();
                    let names_documents = matches!(
                        normalized.as_str(),
                        "documentid"
                            | "documentids"
                            | "docid"
                            | "sourcedocumentid"
                            | "targetdocumentid"
                            | "artifactid"
                    );
                    match value {
                        Value::String(id) if names_documents => out.push(id.trim().to_string()),
                        Value::Array(items) if names_documents => out.extend(
                            items
                                .iter()
                                .filter_map(Value::as_str)
                                .map(|id| id.trim().to_string()),
                        ),
                        _ => visit(value, depth + 1, out),
                    }
                }
            }
            Value::Array(items) => items.iter().for_each(|item| visit(item, depth + 1, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    if let Some(arguments) = arguments {
        visit(arguments, 0, &mut out);
    }
    out.retain(|id| !id.is_empty());
    out
}

/// A public request for a route that names a document in the Trash: 404.
pub(crate) async fn trashed_route_refusal(
    app: &AppHandle,
    matched_path: &str,
    path: &str,
) -> Option<String> {
    let graph = crate::cell_graph_boundary::path_parameter(matched_path, path, "{graph_id}")?;
    let document = crate::cell_graph_boundary::path_parameter(matched_path, path, "{document_id}")
        .or_else(|| crate::cell_graph_boundary::path_parameter(matched_path, path, "{doc_id}"))?;
    let graph = crate::loopback_self_host_routes::decode_segment(graph);
    let document = crate::loopback_self_host_routes::decode_segment(document);
    match trashed_documents(app, &graph).await {
        Ok(trashed) if trashed.contains(document.trim()) => {
            Some(format!("no document {document} in {graph}"))
        }
        Ok(_) => None,
        // The route itself answers for a graph that is not there.
        Err(_) => None,
    }
}

/// The named graphs a public SPARQL query of `graph_id` must not see: those of
/// its trashed documents.
pub(crate) async fn hidden_graphs_for_public(
    app: &AppHandle,
    graph_id: &str,
) -> Result<Vec<String>, String> {
    Ok(trashed_documents(app, graph_id)
        .await?
        .iter()
        .map(|document| crate::rdf_authority::document_projection_graph_iri(graph_id, document))
        .collect())
}

// ─── REST: trash and restore of one's own documents ─────────────────────────

/// The two REST routes a fence may admit: the scopes each needs (lent for the
/// one request) and the document it names.
fn fenced_rest_route(
    method: &axum::http::Method,
    matched_path: &str,
) -> Option<&'static [&'static str]> {
    match (method.as_str(), matched_path) {
        ("DELETE", "/navigation/{graph_id}/documents/{document_id}") => {
            Some(&["workspace.delete.crdt", "artifacts.delete"])
        }
        ("POST", "/navigation/{graph_id}/trash/{artifact_id}/restore") => {
            Some(&["workspace.write.crdt", "artifacts.write"])
        }
        _ => None,
    }
}

/// `None`: not a route a fence admits (public mode's gate decides). Else the
/// fence context to run the handler in, or the refusal.
pub(crate) async fn rest_admission(
    state: &crate::loopback_state::LoopbackState,
    public: &crate::public_mode::PublicMode,
    method: &axum::http::Method,
    matched_path: &str,
    path: &str,
    query: Option<&str>,
    account_token: Option<&str>,
) -> Option<Result<FenceContext, (axum::http::StatusCode, &'static str, String)>> {
    use axum::http::StatusCode;
    let lent = fenced_rest_route(method, matched_path)?;
    public.visitor_folders()?;
    let refuse = |status, code, message: String| Some(Err((status, code, message)));
    let graph = crate::cell_graph_boundary::path_parameter(matched_path, path, "{graph_id}")
        .map(crate::loopback_self_host_routes::decode_segment)
        .unwrap_or_default();
    let document = crate::cell_graph_boundary::path_parameter(
        matched_path,
        path,
        if matched_path.contains("{document_id}") {
            "{document_id}"
        } else {
            "{artifact_id}"
        },
    )
    .map(crate::loopback_self_host_routes::decode_segment)
    .unwrap_or_default();
    if !public.is_public_graph(&graph) {
        return refuse(
            StatusCode::NOT_FOUND,
            "graph_not_found",
            "graph not found".into(),
        );
    }
    if matched_path.ends_with("/restore")
        && !query
            .unwrap_or_default()
            .split('&')
            .any(|pair| pair == "kind=document")
    {
        return refuse(
            StatusCode::FORBIDDEN,
            "public_read_only",
            "a visitor restores only their own documents (?kind=document)".into(),
        );
    }
    let Some(token) = account_token else {
        return refuse(
            StatusCode::FORBIDDEN,
            "public_read_only",
            NoFence::NotAnAccount.message(&graph),
        );
    };
    let accounts = public.accounts()?;
    let identity = match accounts.verify(token) {
        Ok(identity) => identity,
        Err(_) => {
            return refuse(
                StatusCode::UNAUTHORIZED,
                "invalid_account_token",
                "invalid or expired Sophia account token (sign in again)".into(),
            )
        }
    };
    let account = VisitorAccount {
        sub: identity.sub,
        email_verified: identity.email_verified,
    };
    let fence = match fence_for(Some(public), Some(&account), &graph) {
        Ok(fence) => fence,
        Err(why) => return refuse(StatusCode::FORBIDDEN, why.code(), why.message(&graph)),
    };
    let folders = public.visitor_folders()?;
    if let Err(retry) = folders.take_write(&fence.account, &fence.quotas) {
        return refuse(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            format!("slow down: try again in {retry} s"),
        );
    }
    let checked = fence.clone();
    let owned = async {
        let graph_dir = crate::graph_paths::existing_graph_dir(&state.app, &graph)?;
        let room =
            crate::crdt_engine::workspace_ops::workspace_room(&state.app, &graph, &graph_dir)
                .await?;
        let id = document.clone();
        room.with_doc(move |doc| {
            use yrs::Transact;
            let txn = doc.transact();
            own_document(&txn, &checked, &id).map(|_| ())
        })
        .await
    }
    .await;
    if let Err(error) = owned {
        return refuse(StatusCode::FORBIDDEN, "visitor_fence", error);
    }
    Some(Ok(FenceContext {
        fence,
        lent_scopes: lent.iter().map(|scope| (*scope).to_string()).collect(),
    }))
}

/// Run a REST handler under an admitted fence context.
pub(crate) async fn within_context<F: std::future::Future>(
    context: FenceContext,
    future: F,
) -> F::Output {
    CURRENT_FENCE.scope(context, future).await
}

/// Stamp every state of an awareness update a visitor sends (see
/// [`stamp_awareness`]); `None` when nothing is left to merge.
pub(crate) fn stamp_awareness_update(
    app: &AppHandle,
    account: Option<&str>,
    update: yrs::sync::AwarenessUpdate,
) -> Option<yrs::sync::AwarenessUpdate> {
    let state = running_folders(app);
    let folders = state
        .as_ref()
        .and_then(|state| state.self_host.public())
        .and_then(crate::public_mode::PublicMode::visitor_folders);
    let mut clients = HashMap::new();
    for (client, entry) in update.clients {
        if let Some(json_text) = stamp_awareness(folders, account, &entry.json) {
            clients.insert(
                client,
                yrs::sync::awareness::AwarenessUpdateEntry {
                    clock: entry.clock,
                    json: json_text.into(),
                },
            );
        }
    }
    (!clients.is_empty()).then_some(yrs::sync::AwarenessUpdate { clients })
}

/// Rewrite one awareness state a hosted cell's Viewer publishes: its user id
/// and its presence's human id are the lease principal, whatever the client
/// sent (so it cannot speak as, or take over the presence session of,
/// another person), and it is marked `viewer`. The name and colour stay the
/// client's. `None` drops a state that is not a JSON object.
pub(crate) fn stamp_viewer_awareness(principal: &str, json_text: &str) -> Option<String> {
    if json_text == "null" {
        return Some(json_text.to_string());
    }
    let mut state: Value = serde_json::from_str(json_text).ok()?;
    let object = state.as_object_mut()?;
    let mut user = object
        .get("user")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    user.insert("userId".into(), json!(principal));
    user.insert("viewer".into(), json!(true));
    object.insert("user".into(), Value::Object(user));
    if let Some(lifecycle) = object.get_mut("presence").and_then(Value::as_object_mut) {
        lifecycle.insert("humanId".into(), json!(principal));
    }
    serde_json::to_string(&state).ok()
}

/// Stamp every state of an awareness update a hosted Viewer sends (see
/// [`stamp_viewer_awareness`]); `None` when nothing is left to merge.
pub(crate) fn stamp_viewer_awareness_update(
    principal: &str,
    update: yrs::sync::AwarenessUpdate,
) -> Option<yrs::sync::AwarenessUpdate> {
    let mut clients = HashMap::new();
    for (client, entry) in update.clients {
        if let Some(json_text) = stamp_viewer_awareness(principal, &entry.json) {
            clients.insert(
                client,
                yrs::sync::awareness::AwarenessUpdateEntry {
                    clock: entry.clock,
                    json: json_text.into(),
                },
            );
        }
    }
    (!clients.is_empty()).then_some(yrs::sync::AwarenessUpdate { clients })
}

// The end-to-end tests carry `loopback_self_host` in their module name so the
// cells lane's gating passes run them (`ship/lanes/cells.sh`).
#[cfg(all(test, feature = "headless"))]
#[path = "visitor_fence_tests.rs"]
mod loopback_self_host_visitor_fence_tests;

#[cfg(test)]
mod loopback_self_host_visitor_fence_unit {
    use super::*;

    #[test]
    fn presence_colour_matches_shrubbery_hash() {
        // Shrubbery: hash('local-user') % 8 picks the default colour; the same
        // FNV-1a over UTF-16 units must give the same index here.
        let index = (presence_hash("local-user") as usize) % PRESENCE_COLORS.len();
        let mut expected: u32 = 2166136261;
        for unit in "local-user".encode_utf16() {
            expected ^= u32::from(unit);
            expected = expected.wrapping_mul(16777619);
        }
        assert_eq!(index, (expected as usize) % 8);
    }

    #[test]
    fn names_never_carry_an_email_or_claim_the_house() {
        assert!(valid_name("Moo Fan"));
        assert!(!valid_name("carol@example.com"));
        assert!(!valid_name("Owner"));
        assert!(!valid_name("sophia"));
        assert!(!valid_name(""));
        assert!(!valid_name(&"x".repeat(41)));
        assert!(!valid_name("see https://spam.example"));
        // Sealed exam (2026-10-15, D1): invisible characters and look-alikes do not get past the house's names, and
        // no one takes the shape of the names the house gives ("Visitor 7c73").
        for claim in [
            "Owner\u{200b}",
            "O\u{200b}wner",
            "Owner\u{feff}",
            "Owner\u{202e}",
            "\u{41e}wner",
            "0wner",
            "OWNER.",
            "S o p h i a",
            "adm1n",
            "Sophia Labs",
            "Visitor 7c73",
            "visitor-ab12",
            "Moo  Fan",
        ] {
            assert!(!valid_name(claim), "{claim:?} must not be a visitor's name");
        }
        // Round 2 of the exam: the house's words anywhere in a name.
        for claim in [
            "Sophia (Owner)",
            "The Owner",
            "site admin",
            "Moderator Jo",
            "Sophia Fan",
            "Not a visitor",
        ] {
            assert!(!valid_name(claim), "{claim:?} must not be a visitor's name");
        }
        for fine in ["María José", "O'Brien", "Ana-Lía", "Moo Fan 2"] {
            assert!(valid_name(fine), "{fine:?} is a name");
        }
    }

    #[test]
    fn a_name_another_account_holds_stays_theirs() {
        let dir =
            std::env::temp_dir().join(format!("garden-visitor-names-{}", uuid::Uuid::new_v4()));
        let graphs: BTreeSet<String> = ["g".to_string()].into();
        let file: Vec<VisitorFolderFile> =
            serde_json::from_value(json!([{ "graphId": "g", "folderId": "f" }])).unwrap();
        let folders = VisitorFolders::from_file(file, &graphs, &dir)
            .unwrap()
            .unwrap();
        let alice = folders.account_id("sub-alice");
        let bob = folders.account_id("sub-bob");
        assert_eq!(
            folders.bind_presence(&alice, Some("Moo Fan"), None).name,
            "Moo Fan"
        );
        let alice_default = folders.presence_of(&bob).name.clone();
        let before = folders.presence_of(&bob);
        for claim in ["Moo Fan", "moo fan", "MOO-FAN", "M00 Fan"] {
            assert_eq!(
                folders.bind_presence(&bob, Some(claim), None),
                before,
                "{claim:?} is Alice's"
            );
        }
        assert_eq!(
            folders.bind_presence(&bob, Some("Cow Poet"), None).name,
            "Cow Poet"
        );
        assert!(alice_default.starts_with("Visitor "));
        // Alice keeps hers, and may set it again.
        assert_eq!(
            folders.bind_presence(&alice, Some("Moo Fan"), None).name,
            "Moo Fan"
        );
        // Round 2 of the exam: a name an account gave up stays its own (its documents and comments carry it).
        assert_eq!(
            folders.bind_presence(&alice, Some("Cud Chewer"), None).name,
            "Cud Chewer"
        );
        let before = folders.presence_of(&bob);
        assert_eq!(
            folders.bind_presence(&bob, Some("Moo Fan"), None),
            before,
            "Alice's earlier name stays hers"
        );
        assert_eq!(
            folders.bind_presence(&alice, Some("Moo Fan"), None).name,
            "Moo Fan",
            "and she may take it back"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_gardends_predicates() {
        assert!(builtin_predicate(None));
        assert!(builtin_predicate(Some("supports")));
        assert!(builtin_predicate(Some(
            "http://mnemosyne.ai/vocab#supports"
        )));
        assert!(!builtin_predicate(Some("owl:sameAs")));
        assert!(!builtin_predicate(Some(
            "http://www.w3.org/2002/07/owl#sameAs"
        )));
        assert!(!builtin_predicate(Some(
            "http://evil.example/vocab#supports"
        )));
    }

    #[test]
    fn the_tiptap_policy_refuses_images_queries_and_odd_links() {
        let ok = json!({"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"hi","marks":[{"type":"link","attrs":{"href":"https://example.com"}}]}]}]});
        let (mut links, mut chars) = (0, 0);
        assert!(check_tiptap(&ok, "g", &mut links, &mut chars).is_ok());
        assert_eq!(links, 1);
        for bad in [
            json!({"type":"doc","content":[{"type":"image","attrs":{"src":"https://tracker.example/p.gif"}}]}),
            json!({"type":"doc","content":[{"type":"queryBlock","attrs":{"query":"SELECT * {?s ?p ?o}"}}]}),
            json!({"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"x","marks":[{"type":"link","attrs":{"href":"javascript:alert(1)"}}]}]}]}),
            json!({"type":"doc","content":[{"type":"paragraph","content":[{"type":"wikilink","attrs":{"targetGraphId":"owner-private"}}]}]}),
            json!({"type":"doc","content":[{"type":"opaqueBlock"}]}),
            // An embed can point at an object in a graph the visitor cannot read.
            json!({"type":"doc","content":[{"type":"objectEmbed","attrs":{"iri":"urn:sophia:object:owner-private"}}]}),
        ] {
            let (mut links, mut chars) = (0, 0);
            assert!(
                check_tiptap(&bad, "g", &mut links, &mut chars).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn awareness_is_stamped_with_the_account() {
        let dir =
            std::env::temp_dir().join(format!("garden-visitor-presence-{}", uuid::Uuid::new_v4()));
        let graphs: BTreeSet<String> = ["g".to_string()].into();
        let file: Vec<VisitorFolderFile> =
            serde_json::from_value(json!([{ "graphId": "g", "folderId": "f" }])).unwrap();
        let folders = VisitorFolders::from_file(file, &graphs, &dir)
            .unwrap()
            .unwrap();
        let account = folders.account_id("sub-1");
        let forged = json!({
            "user": { "userId": "owner", "name": "Owner", "color": "#123456", "type": "agent" },
            "presence": { "humanId": "owner", "clientId": "c" }
        })
        .to_string();
        let stamped: Value = serde_json::from_str(
            &stamp_awareness(Some(&folders), Some(&account), &forged).unwrap(),
        )
        .unwrap();
        assert_eq!(stamped["user"]["userId"], json!(account));
        assert_ne!(
            stamped["user"]["name"],
            json!("Owner"),
            "a reserved name is not bound"
        );
        assert_eq!(
            stamped["user"]["color"],
            json!("#123456"),
            "a valid colour is the account's choice"
        );
        assert_eq!(stamped["user"]["type"], json!("human"));
        assert_eq!(stamped["user"]["visitor"], json!(true));
        assert_eq!(stamped["presence"]["humanId"], json!(account));
        let renamed = json!({ "user": { "name": "Moo Fan", "color": "#2563eb" } }).to_string();
        let stamped: Value = serde_json::from_str(
            &stamp_awareness(Some(&folders), Some(&account), &renamed).unwrap(),
        )
        .unwrap();
        assert_eq!(stamped["user"]["name"], json!("Moo Fan"));
        assert_eq!(
            folders.presence_of(&account).name,
            "Moo Fan",
            "the name is bound to the account"
        );
        assert!(
            stamp_awareness(None, None, &renamed).is_none(),
            "an anonymous visitor publishes no presence"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
