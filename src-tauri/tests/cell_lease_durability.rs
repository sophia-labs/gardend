//! U8 cross-process lease acceptance against a real compiled `gardend` and a
//! second, real TCP+SQLite implementation of the authority wire contract.
//!
//! Slice-3 legs (a) and (c) are ack-path RPO claims deferred to U1's log and
//! intentionally remain out of scope here.

mod support;

use garden_lib::headless::durability::{boot_repair, BootRepairOutcome};
use reqwest::StatusCode;
use std::{
    collections::{BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
    time::{Duration, Instant},
};
use support::{
    gardend_process::{self as process, GardendConfig, LoopbackEndpoint, ScratchDir},
    lease_authority::{ClaimResult, LeaseAuthority},
};

const TOKEN: &str = "u8-real-process-token";
static PROCESS_TEST_LOCK: Mutex<()> = Mutex::new(());

fn process_test_lock() -> MutexGuard<'static, ()> {
    PROCESS_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write_current(dir: &Path, seq: u64) {
    fs::write(dir.join("CURRENT"), format!("snap-{seq:06}")).unwrap();
}

fn snap(dir: &Path, seq: u64) {
    let path = dir.join(format!("snap-{seq:06}"));
    fs::create_dir_all(&path).unwrap();
    fs::write(path.join("marker.txt"), format!("snapshot-{seq}")).unwrap();
}

fn valid_snap(dir: &Path, seq: u64, graph_id: &str) {
    let path = dir.join(format!("snap-{seq:06}"));
    process::prime_graph(&path, graph_id);
    fs::write(path.join("marker.txt"), format!("snapshot-{seq}")).unwrap();
}

fn lease_env(
    authority: &LeaseAuthority,
    graph_id: &str,
    holder: &str,
    claim: &ClaimResult,
    mode: &str,
    last_snap: Option<u64>,
) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = [
        ("GARDEN_DURABLE_EPOCH", claim.epoch.to_string()),
        ("GARDEN_CELL_MACHINE_RUN_ID", holder.to_string()),
        ("GARDEN_LEASE_URL", authority.base_url()),
        ("GARDEN_LOOPBACK_TOKEN", TOKEN.to_string()),
        ("GARDEN_LEASE_MODE", mode.to_string()),
        ("GARDEN_LEASE_RENEW_MS", "100".to_string()),
        ("GARDEN_LEASE_MARGIN_MS", "300".to_string()),
        ("GARDEN_LEASE_FENCED_MAX_MS", "120000".to_string()),
        ("GARDEN_LEASE_PUBLISH_TIMEOUT_MS", "2000".to_string()),
        ("GARDEN_FLUSH_DEBOUNCE_SECONDS", "1".to_string()),
        ("GARDEN_FLUSH_INTERVAL_SECONDS", "1".to_string()),
        ("RUST_LOG", "info".to_string()),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value))
    .collect();
    // `GARDEN_CELL_GRAPH_ID` is also set by `spawn_gardend`; keeping it here
    // makes the complete lease envelope obvious at the call site.
    env.insert("GARDEN_CELL_GRAPH_ID".into(), graph_id.into());
    if let Some(last_snap) = last_snap {
        env.insert("GARDEN_LEASE_LAST_SNAP".into(), last_snap.to_string());
    }
    if let Some(pending_snap) = claim.pending_snap {
        env.insert("GARDEN_LEASE_PENDING_SNAP".into(), pending_snap.to_string());
    }
    env
}

fn spawn(
    root: &ScratchDir,
    label: &str,
    profile: PathBuf,
    durable: PathBuf,
    graph_id: &str,
    extra_env: HashMap<String, String>,
) -> (process::ChildGuard, PathBuf) {
    process::spawn_gardend(GardendConfig {
        profile_dir: profile,
        durable_dir: durable,
        graph_id: graph_id.into(),
        loopback_host: "127.0.0.1".into(),
        loopback_port: 0,
        extra_env,
        log_dir: root.child("logs"),
        log_label: label.into(),
    })
}

fn endpoint(profile: &Path) -> LoopbackEndpoint {
    LoopbackEndpoint::from_profile(profile, Duration::from_secs(30)).unwrap_or_else(|| {
        let log_dir = profile
            .parent()
            .expect("profile has scratch parent")
            .join("logs");
        let diagnostics = fs::read_dir(&log_dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                fs::read_to_string(entry.path())
                    .ok()
                    .map(|body| format!("--- {} ---\n{body}", entry.path().display()))
            })
            .collect::<Vec<_>>()
            .join("\n");
        panic!("gardend loopback manifest/readiness:\n{diagnostics}")
    })
}

async fn put(
    endpoint: &LoopbackEndpoint,
    graph_id: &str,
    document_id: &str,
    title: &str,
    content: &str,
) -> serde_json::Value {
    let (status, value) = endpoint
        .put_document(graph_id, document_id, title, content)
        .await;
    assert_eq!(status, StatusCode::OK, "PUT response: {value}");
    value
}

async fn put_allow_terminal_disconnect(
    endpoint: &LoopbackEndpoint,
    graph_id: &str,
    document_id: &str,
    title: &str,
    content: &str,
) {
    match endpoint
        .try_put_document(graph_id, document_id, title, content)
        .await
    {
        Ok((status, value)) => assert_eq!(status, StatusCode::OK, "PUT response: {value}"),
        Err(error) => {
            // A terminal lease event can close the process after the local
            // mutation is admitted but before Hyper finishes the response.
            // The surrounding test proves the intended exit/durable outcome;
            // transport acknowledgement is deliberately not the fence.
            eprintln!("terminal process closed the triggering PUT response: {error}");
        }
    }
}

async fn get(endpoint: &LoopbackEndpoint, graph_id: &str, document_id: &str) -> serde_json::Value {
    let (status, value) = endpoint.get_document(graph_id, document_id).await;
    assert_eq!(status, StatusCode::OK, "GET response: {value}");
    value
}

async fn wait_for_health_status(
    endpoint: &LoopbackEndpoint,
    expected: &str,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if endpoint.health().await.1["status"] == expected {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    endpoint.health().await.1["status"] == expected
}

fn terminate_cleanly(child: &mut process::ChildGuard) {
    process::send_sigterm(&child.child);
    let status = process::wait_with_timeout(&mut child.child, Duration::from_secs(30))
        .expect("gardend should stop after SIGTERM");
    assert_eq!(
        status.code(),
        Some(0),
        "unexpected gardend status: {status}"
    );
}

#[test]
fn z5_first_rollout_never_quarantines() {
    let root = ScratchDir::new("z5-pure");
    let dir = root.child("seeded");
    snap(&dir, 1);
    write_current(&dir, 1);
    assert_eq!(
        boot_repair(&dir, Some(1), None, false).unwrap(),
        BootRepairOutcome::Clean
    );
    assert_eq!(
        fs::read_to_string(dir.join("CURRENT")).unwrap().trim(),
        "snap-000001"
    );
    assert!(process::list_orphan_dirs(&dir).is_empty());

    let fresh = root.child("unseeded");
    snap(&fresh, 1);
    write_current(&fresh, 1);
    assert_eq!(
        boot_repair(&fresh, None, None, false).unwrap(),
        BootRepairOutcome::SkippedNoLastSnap
    );
    assert_eq!(
        fs::read_to_string(fresh.join("CURRENT")).unwrap().trim(),
        "snap-000001"
    );
    assert!(process::list_orphan_dirs(&fresh).is_empty());
}

#[test]
fn z4_escaped_publish_is_quarantined_and_rewind_is_restored() {
    let root = ScratchDir::new("z4-pure");
    let dir = root.child("quarantine");
    for seq in 1..=3 {
        snap(&dir, seq);
    }
    write_current(&dir, 3);
    let outcome = boot_repair(&dir, Some(1), None, false).unwrap();
    let orphan_dir_name = match outcome {
        BootRepairOutcome::Quarantined {
            restored_to,
            orphaned,
            orphan_dir_name,
        } => {
            assert_eq!(restored_to, 1);
            assert_eq!(orphaned, vec![2, 3]);
            orphan_dir_name
        }
        other => panic!("unexpected outcome: {other:?}"),
    };
    assert_eq!(process::durable_current_seq(&dir), Some(1));
    let orphan = dir.join(orphan_dir_name);
    assert!(orphan.join("snap-000002/marker.txt").is_file());
    assert!(orphan.join("snap-000003/marker.txt").is_file());
    assert!(!dir.join("snap-000002").exists());
    assert!(!dir.join("snap-000003").exists());

    let fresh = root.child("restore");
    snap(&fresh, 1);
    snap(&fresh, 2);
    write_current(&fresh, 1);
    assert_eq!(
        boot_repair(&fresh, Some(2), None, false).unwrap(),
        BootRepairOutcome::Restored { restored_to: 2 }
    );
    assert_eq!(process::durable_current_seq(&fresh), Some(2));
    assert!(process::list_orphan_dirs(&fresh).is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn z4_z5_real_process_boot_runs_repair_and_first_rollout_suppression() {
    let _guard = process_test_lock();
    let root = ScratchDir::new("z4-z5-process");
    let authority = LeaseAuthority::start(TOKEN, 20_000, 0);

    // Z4, S>L: boot must quarantine escaped snapshots before the loopback
    // server appears.
    let graph_q = "z4-quarantine";
    let durable_q = root.child("durable-q");
    for seq in 1..=3 {
        valid_snap(&durable_q, seq, graph_q);
    }
    write_current(&durable_q, 3);
    authority.seed_expired(graph_q, 0, 1);
    let claim_q = authority.claim(graph_q, "holder-q", 1);
    let profile_q = root.child("profile-q");
    let mut env_q = lease_env(
        &authority,
        graph_q,
        "holder-q",
        &claim_q,
        "enforce",
        Some(1),
    );
    env_q.insert("GARDEN_FLUSH_DEBOUNCE_SECONDS".into(), "3600".into());
    env_q.insert("GARDEN_FLUSH_INTERVAL_SECONDS".into(), "3600".into());
    let (mut child_q, _) = spawn(
        &root,
        "z4-q",
        profile_q.clone(),
        durable_q.clone(),
        graph_q,
        env_q,
    );
    endpoint(&profile_q);
    assert_eq!(process::durable_current_seq(&durable_q), Some(1));
    assert_eq!(process::list_orphan_dirs(&durable_q).len(), 1);
    terminate_cleanly(&mut child_q);

    // Z4, S<L: boot must restore the authority's high-water mark.
    let graph_r = "z4-restore";
    let durable_r = root.child("durable-r");
    valid_snap(&durable_r, 1, graph_r);
    valid_snap(&durable_r, 2, graph_r);
    write_current(&durable_r, 1);
    authority.seed_expired(graph_r, 0, 2);
    let claim_r = authority.claim(graph_r, "holder-r", 2);
    let profile_r = root.child("profile-r");
    let mut env_r = lease_env(
        &authority,
        graph_r,
        "holder-r",
        &claim_r,
        "enforce",
        Some(2),
    );
    env_r.insert("GARDEN_FLUSH_DEBOUNCE_SECONDS".into(), "3600".into());
    env_r.insert("GARDEN_FLUSH_INTERVAL_SECONDS".into(), "3600".into());
    let (mut child_r, _) = spawn(
        &root,
        "z4-r",
        profile_r.clone(),
        durable_r.clone(),
        graph_r,
        env_r,
    );
    endpoint(&profile_r);
    assert_eq!(process::durable_current_seq(&durable_r), Some(2));
    terminate_cleanly(&mut child_r);

    // Z5 seeded and unseeded first-rollout boots both leave CURRENT alone and
    // never create a quarantine.
    for (suffix, include_last_snap) in [("seeded", true), ("unseeded", false)] {
        let graph = format!("z5-{suffix}");
        let durable = root.child(&format!("durable-{suffix}"));
        valid_snap(&durable, 1, &graph);
        write_current(&durable, 1);
        authority.seed_expired(&graph, 0, 1);
        let holder = format!("holder-{suffix}");
        let claim = authority.claim(&graph, &holder, 1);
        let profile = root.child(&format!("profile-{suffix}"));
        let mut env = lease_env(
            &authority,
            &graph,
            &holder,
            &claim,
            "enforce",
            include_last_snap.then_some(1),
        );
        env.insert("GARDEN_FLUSH_DEBOUNCE_SECONDS".into(), "3600".into());
        env.insert("GARDEN_FLUSH_INTERVAL_SECONDS".into(), "3600".into());
        let (mut child, _) = spawn(
            &root,
            &format!("z5-{suffix}"),
            profile.clone(),
            durable.clone(),
            &graph,
            env,
        );
        endpoint(&profile);
        assert_eq!(process::durable_current_seq(&durable), Some(1));
        assert!(process::list_orphan_dirs(&durable).is_empty());
        terminate_cleanly(&mut child);
    }
}

async fn run_zombie_gate_case(label: &str, bypass_gate_a: bool) {
    let root = ScratchDir::new(label);
    let graph = format!("{label}-graph");
    let durable = root.child("durable");
    let authority = LeaseAuthority::start(TOKEN, 1_200, 0);
    authority.seed_expired(&graph, 4, 0);

    let holder_a = format!("{label}-a");
    let claim_a = authority.claim(&graph, &holder_a, 0);
    assert_eq!(claim_a.epoch, 5);
    let profile_a = root.child("profile-a");
    process::prime_graph(&profile_a, &graph);
    let mut env_a = lease_env(&authority, &graph, &holder_a, &claim_a, "enforce", Some(0));
    if bypass_gate_a {
        env_a.insert("GARDEN_LEASE_TEST_BYPASS_GATE_A".into(), "1".into());
    }
    let (mut child_a, logs) = spawn(
        &root,
        "a",
        profile_a.clone(),
        durable.clone(),
        &graph,
        env_a,
    );
    let endpoint_a = endpoint(&profile_a);
    put(&endpoint_a, &graph, "state", "epoch-five", "A").await;
    assert!(
        process::wait_for_current_seq(&durable, 1, Duration::from_secs(20)),
        "A never published snap 1:\n{}",
        process::read_log(&logs, "a", "stderr")
    );
    assert_eq!(authority.describe(&graph).unwrap().last_snap, 1);

    authority.pause_renew(&holder_a, true);
    assert!(authority.wait_until_expired(&graph, Duration::from_secs(5)));
    let claim_b = authority.claim(&graph, &format!("{label}-b"), 1);
    assert_eq!(claim_b.epoch, 6);
    let holder_b = format!("{label}-b");
    let profile_b = root.child("profile-b");
    let env_b = lease_env(&authority, &graph, &holder_b, &claim_b, "enforce", Some(1));
    let (mut child_b, _) = spawn(
        &root,
        "b",
        profile_b.clone(),
        durable.clone(),
        &graph,
        env_b,
    );
    let endpoint_b = endpoint(&profile_b);
    let expected_b = put(&endpoint_b, &graph, "state", "epoch-six", "B survives").await;
    assert!(
        process::wait_for_current_seq(&durable, 2, Duration::from_secs(20)),
        "B never published snap 2"
    );
    assert_eq!(authority.describe(&graph).unwrap().last_snap, 2);

    // OR-6: the stale cell may acknowledge its local/in-memory write. The
    // durability gates, not API refusal, close the zombie.
    put_allow_terminal_disconnect(
        &endpoint_a,
        &graph,
        "state",
        "stale-epoch-five",
        "must not survive",
    )
    .await;
    let gate_log = if bypass_gate_a {
        "durable flush FENCED at Gate B"
    } else {
        "durable flush FENCED at Gate A"
    };
    assert!(
        process::wait_for_log(&logs, "a", "stderr", gate_log, Duration::from_secs(10)),
        "missing {gate_log} testimony:\n{}",
        process::read_log(&logs, "a", "stderr")
    );
    if bypass_gate_a {
        assert!(
            process::read_log(&logs, "a", "stderr").contains("TEST ONLY: bypassing lease Gate A")
        );
    }
    // B may legitimately publish another snapshot for recovered/background
    // state while this proof runs, so an exact sequence of 2 is brittle. The
    // safety invariant is that every post-successor advance is B-authorized
    // and CURRENT converges to the authority high-water mark; stale A has no
    // accepted intent/commit after B's claim.
    assert!(process::wait_until(Duration::from_secs(5), || {
        process::durable_current_seq(&durable)
            == authority.describe(&graph).map(|row| row.last_snap)
    }));
    let current_after_stale = process::durable_current_seq(&durable).expect("CURRENT");
    assert!(current_after_stale >= 2);
    let events = authority.events(&graph);
    let successor_claim_event = events
        .iter()
        .find(|event| event.kind == "claim" && event.holder == holder_b)
        .expect("successor claim event");
    assert!(
        !events.iter().any(|event| {
            event.id > successor_claim_event.id
                && event.holder == holder_a
                && matches!(event.kind.as_str(), "intent" | "commit")
        }),
        "stale holder published after successor claim: {events:?}"
    );
    assert!(process::list_snap_dirs(&durable).iter().all(|name| {
        name.strip_prefix("snap-")
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|seq| seq <= current_after_stale)
    }));

    // Gate A has no authority round-trip, so let the real renew endpoint
    // supply terminal 409 evidence. The Gate-B bypass case may already have
    // learned the newer epoch from its CAS response.
    authority.pause_renew(&holder_a, false);
    let status_a = process::wait_with_timeout(&mut child_a.child, Duration::from_secs(15))
        .expect("stale A should exit on positive 409 evidence");
    assert_eq!(status_a.code(), Some(4));
    assert!(process::read_log(&logs, "a", "stderr")
        .contains("skipping the final durable flush entirely"));
    assert!(process::durable_current_seq(&durable).is_some_and(|seq| seq >= current_after_stale));

    let observed_b = get(&endpoint_b, &graph, "state").await;
    assert_eq!(
        process::semantic_document(&observed_b),
        process::semantic_document(&expected_b)
    );
    terminate_cleanly(&mut child_b);

    let claim_c = authority.claim(&graph, &format!("{label}-c"), 2);
    let holder_c = format!("{label}-c");
    let profile_c = root.child("profile-c");
    let env_c = lease_env(
        &authority,
        &graph,
        &holder_c,
        &claim_c,
        "enforce",
        Some(claim_c.last_snap),
    );
    let (mut child_c, _) = spawn(
        &root,
        "c",
        profile_c.clone(),
        durable.clone(),
        &graph,
        env_c,
    );
    let endpoint_c = endpoint(&profile_c);
    let observed_c = get(&endpoint_c, &graph, "state").await;
    assert_eq!(
        process::semantic_document(&observed_c),
        process::semantic_document(&expected_b),
        "semantic state after successor hydrate must be B, never stale A"
    );
    terminate_cleanly(&mut child_c);
}

#[tokio::test(flavor = "current_thread")]
async fn z1_zombie_epoch_closes_gate_a_and_independent_gate_b_bypass() {
    let _guard = process_test_lock();
    run_zombie_gate_case("z1-gate-a", false).await;
    run_zombie_gate_case("z1-gate-b", true).await;
}

#[tokio::test(flavor = "current_thread")]
async fn z1_gate_c_commit_409_without_epoch_is_terminal_and_repaired() {
    let _guard = process_test_lock();
    let root = ScratchDir::new("z1-gate-c");
    let graph = "z1-gate-c";
    let durable = root.child("durable");
    let authority = LeaseAuthority::start(TOKEN, 20_000, 0);
    authority.seed_expired(graph, 4, 0);
    let claim = authority.claim(graph, "gate-c-a", 0);
    assert_eq!(claim.epoch, 5);
    let profile_a = root.child("profile-a");
    process::prime_graph(&profile_a, graph);
    let (mut child_a, logs) = spawn(
        &root,
        "gate-c-a",
        profile_a.clone(),
        durable.clone(),
        graph,
        lease_env(&authority, graph, "gate-c-a", &claim, "enforce", Some(0)),
    );
    let endpoint_a = endpoint(&profile_a);
    put(&endpoint_a, graph, "state", "committed", "safe").await;
    assert!(process::wait_for_current_seq(
        &durable,
        1,
        Duration::from_secs(20)
    ));
    // CURRENT is deliberately rewritten before Gate C commits `last_snap`.
    // Wait for both halves of the initial publish before injecting delay into
    // the second one; asserting immediately after the local pointer appears
    // races the real HTTP commit on a loaded test host.
    assert!(
        process::wait_until(Duration::from_secs(5), || {
            authority
                .describe(graph)
                .is_some_and(|row| row.last_snap == 1)
        }),
        "initial Gate C commit never reached the authority"
    );

    authority.omit_epoch_on_publish_conflict(true);
    authority.supersede_on_next_commit("gate-c-a");
    put_allow_terminal_disconnect(
        &endpoint_a,
        graph,
        "state",
        "commit-rejected",
        "must survive successor repair",
    )
    .await;
    let status = process::wait_with_timeout(&mut child_a.child, Duration::from_secs(20))
        .expect("Gate C 409 should terminate the process");
    assert_eq!(status.code(), Some(4));
    let stderr = process::read_log(&logs, "gate-c-a", "stderr");
    assert!(
        stderr.contains("Gate C publish_commit"),
        "missing Gate C testimony:\n{stderr}"
    );
    assert!(
        stderr.contains("skipping the final durable flush entirely"),
        "terminal shutdown must not final-flush:\n{stderr}"
    );
    let row = authority.describe(graph).unwrap();
    assert_eq!(row.epoch, 6);
    assert_eq!(row.last_snap, 1);
    assert_eq!(row.pending_snap, Some(2));
    assert_eq!(process::durable_current_seq(&durable), Some(2));

    // The injected successor is a real authority row. Booting its real
    // process with the claim's ALL_OLD pending carrier must accept snap 2,
    // publish its matching commit under the successor token, and recover the
    // state that crossed CURRENT before stale Gate C was refused.
    let successor_holder = "successor-after-gate-c-a";
    let successor_claim = ClaimResult {
        epoch: row.epoch,
        effective_at_ms: row.effective_at,
        last_snap: row.last_snap,
        pending_snap: row.pending_snap,
    };
    let profile_b = root.child("profile-b");
    let (mut child_b, _) = spawn(
        &root,
        "gate-c-b",
        profile_b.clone(),
        durable.clone(),
        graph,
        lease_env(
            &authority,
            graph,
            successor_holder,
            &successor_claim,
            "enforce",
            Some(1),
        ),
    );
    let endpoint_b = endpoint(&profile_b);
    assert_eq!(process::durable_current_seq(&durable), Some(2));
    assert!(process::list_orphan_dirs(&durable).is_empty());
    let repaired_row = authority.describe(graph).unwrap();
    assert_eq!(repaired_row.last_snap, 2);
    assert_eq!(repaired_row.pending_snap, None);
    let recovered = get(&endpoint_b, graph, "state").await;
    let recovered_semantic = process::semantic_document(&recovered);
    assert_eq!(recovered_semantic["title"], "commit-rejected");
    assert!(
        recovered_semantic["blocks"]
            .to_string()
            .contains("must survive successor repair"),
        "successor did not hydrate the accepted pending snapshot: {recovered_semantic}"
    );
    terminate_cleanly(&mut child_b);
}

#[tokio::test(flavor = "current_thread")]
async fn gate_c_unavailable_latches_gate_a_and_pending_mismatch_restarts_after_recovery() {
    let _guard = process_test_lock();
    let root = ScratchDir::new("gate-c-unavailable");
    let graph = "gate-c-unavailable";
    let holder = "gate-c-unavailable-a";
    let durable = root.child("durable");
    let authority = LeaseAuthority::start(TOKEN, 5_000, 0);
    authority.seed_expired(graph, 0, 0);
    let claim = authority.claim(graph, holder, 0);
    let profile = root.child("profile");
    process::prime_graph(&profile, graph);
    let mut env = lease_env(&authority, graph, holder, &claim, "enforce", Some(0));
    env.insert("GARDEN_LEASE_RENEW_MS".into(), "50".into());
    let (mut child, logs) = spawn(
        &root,
        "gate-c-unavailable",
        profile.clone(),
        durable.clone(),
        graph,
        env,
    );
    let endpoint_a = endpoint(&profile);
    put(&endpoint_a, graph, "state", "baseline", "committed").await;
    assert!(process::wait_for_current_seq(
        &durable,
        1,
        Duration::from_secs(20)
    ));
    assert_eq!(authority.describe(graph).unwrap().last_snap, 1);

    // Let intent(2) and CURRENT(2) land, then make only commit(2)
    // unavailable. Pausing renew prevents an unrelated success from clearing
    // the resulting recoverable fence before Gate A is exercised.
    authority.pause_renew(holder, true);
    authority.fail_next_commit(holder);
    put(
        &endpoint_a,
        graph,
        "state",
        "pending-two",
        "original unresolved bridge",
    )
    .await;
    assert!(process::wait_for_current_seq(
        &durable,
        2,
        Duration::from_secs(20)
    ));
    assert!(process::wait_for_log(
        &logs,
        "gate-c-unavailable",
        "stderr",
        "Gate C publish_commit(seq=2) failed",
        Duration::from_secs(10),
    ));
    assert!(
        wait_for_health_status(&endpoint_a, "fenced", Duration::from_secs(5)).await,
        "Gate-C unavailability never latched the recoverable fence"
    );
    let pending = authority.describe(graph).unwrap();
    assert_eq!(pending.last_snap, 1);
    assert_eq!(pending.pending_snap, Some(2));

    // A second local mutation is admissible for reads, but while the
    // recoverable fence remains latched Gate A must refuse before a fresh
    // intent can overwrite P=2.
    put(
        &endpoint_a,
        graph,
        "state",
        "blocked-three",
        "must not overwrite pending two",
    )
    .await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(
        child.is_running(),
        "recoverable fence must keep reads alive"
    );
    assert!(process::read_log(&logs, "gate-c-unavailable", "stderr")
        .contains("durable flush FENCED at Gate A"));
    assert_eq!(process::durable_current_seq(&durable), Some(2));
    let still_pending = authority.describe(graph).unwrap();
    assert_eq!(still_pending.last_snap, 1);
    assert_eq!(still_pending.pending_snap, Some(2));
    assert!(
        !authority
            .events(graph)
            .iter()
            .any(|event| event.seq == Some(3)),
        "latched Gate A allowed a third bridge mutation"
    );

    // A successful renew may clear the transient fence, but it cannot erase
    // the unresolved bridge. The next real Gate-B intent(3) is therefore
    // `lease_contended`; Garden must restart for boot repair, never replace P.
    authority.pause_renew(holder, false);
    let status = process::wait_with_timeout(&mut child.child, Duration::from_secs(20))
        .expect("pending mismatch should terminate for successor repair");
    assert_eq!(status.code(), Some(4));
    let stderr = process::read_log(&logs, "gate-c-unavailable", "stderr");
    assert!(
        stderr.contains("durable flush FENCED at Gate B"),
        "missing non-overwrite Gate-B testimony:\n{stderr}"
    );
    assert!(stderr.contains("skipping the final durable flush entirely"));
    let final_row = authority.describe(graph).unwrap();
    assert_eq!(final_row.epoch, claim.epoch);
    assert_eq!(final_row.last_snap, 1);
    assert_eq!(final_row.pending_snap, Some(2));
    assert_eq!(process::durable_current_seq(&durable), Some(2));

    // A real successor claim receives P=2 from the authority's old image.
    // Its real Gardend boot must take AcceptPending, commit P under the new
    // token, and hydrate the snapshot that crossed CURRENT before the 503.
    assert!(
        authority.wait_until_expired(graph, Duration::from_secs(7)),
        "terminal holder did not age out for successor claim"
    );
    let successor_holder = "gate-c-unavailable-b";
    let successor_claim = authority.claim(graph, successor_holder, 0);
    assert_eq!(successor_claim.epoch, claim.epoch + 1);
    assert_eq!(successor_claim.last_snap, 1);
    assert_eq!(successor_claim.pending_snap, Some(2));
    let successor_profile = root.child("successor-profile");
    let (mut successor, _) = spawn(
        &root,
        "gate-c-unavailable-successor",
        successor_profile.clone(),
        durable.clone(),
        graph,
        lease_env(
            &authority,
            graph,
            successor_holder,
            &successor_claim,
            "enforce",
            Some(successor_claim.last_snap),
        ),
    );
    let successor_endpoint = endpoint(&successor_profile);
    let repaired = authority.describe(graph).unwrap();
    assert_eq!(repaired.last_snap, 2);
    assert_eq!(repaired.pending_snap, None);
    assert_eq!(process::durable_current_seq(&durable), Some(2));
    assert!(process::list_orphan_dirs(&durable).is_empty());
    let recovered = get(&successor_endpoint, graph, "state").await;
    let semantic = process::semantic_document(&recovered);
    assert_eq!(semantic["title"], "pending-two");
    assert!(
        semantic["blocks"]
            .to_string()
            .contains("original unresolved bridge"),
        "AcceptPending successor hydrated the wrong state: {semantic}"
    );
    terminate_cleanly(&mut successor);
}

#[tokio::test(flavor = "current_thread")]
async fn gate_b_same_epoch_retiring_lease_lost_is_immediately_terminal() {
    let _guard = process_test_lock();
    let root = ScratchDir::new("gate-b-retiring");
    let graph = "gate-b-retiring";
    let durable = root.child("durable");
    let profile = root.child("profile");
    process::prime_graph(&profile, graph);
    let authority = LeaseAuthority::start(TOKEN, 20_000, 0);
    let claim = authority.claim(graph, "retiring-holder", 0);
    let mut env = lease_env(
        &authority,
        graph,
        "retiring-holder",
        &claim,
        "enforce",
        Some(0),
    );
    // Hold renew at transient and bypass only cached Gate A so the real
    // publish-intent CAS independently proves same-epoch retiring is terminal.
    env.insert("GARDEN_LEASE_TEST_BYPASS_GATE_A".into(), "1".into());
    env.insert("GARDEN_FLUSH_DEBOUNCE_SECONDS".into(), "2".into());
    env.insert("GARDEN_FLUSH_INTERVAL_SECONDS".into(), "2".into());
    let (mut child, logs) = spawn(
        &root,
        "retiring",
        profile.clone(),
        durable.clone(),
        graph,
        env,
    );
    let endpoint = endpoint(&profile);
    assert!(process::wait_for_current_seq(
        &durable,
        1,
        Duration::from_secs(20)
    ));
    assert_eq!(authority.describe(graph).unwrap().last_snap, 1);
    authority.pause_renew("retiring-holder", true);
    put(
        &endpoint,
        graph,
        "state",
        "same epoch retired",
        "must never publish",
    )
    .await;
    // Retire only after the public write has completed. Retiring before the
    // request can let an unrelated initial periodic flush terminate the cell
    // while the HTTP response is still in flight, obscuring the intended
    // dirty-write Gate-B proof.
    authority.retire(graph);

    let status = process::wait_with_timeout(&mut child.child, Duration::from_secs(20))
        .expect("same-epoch Gate B lease_lost should terminate immediately");
    assert_eq!(status.code(), Some(4));
    let stderr = process::read_log(&logs, "retiring", "stderr");
    assert!(stderr.contains("FENCED at Gate B"), "{stderr}");
    assert!(stderr.contains("LeaseLost"), "{stderr}");
    assert_eq!(process::durable_current_seq(&durable), Some(1));
    assert_eq!(authority.describe(graph).unwrap().last_snap, 1);
}

#[tokio::test(flavor = "current_thread")]
async fn z2_two_writers_hold_current_monotone_for_full_sixty_seconds() {
    let _guard = process_test_lock();
    let root = ScratchDir::new("z2");
    let graph = "z2-two-writers";
    let durable = root.child("durable");
    let authority = LeaseAuthority::start(TOKEN, 1_200, 0);
    authority.seed_expired(graph, 0, 0);

    let claim_a = authority.claim(graph, "z2-a", 0);
    let profile_a = root.child("profile-a");
    process::prime_graph(&profile_a, graph);
    let (mut child_a, _) = spawn(
        &root,
        "z2-a",
        profile_a.clone(),
        durable.clone(),
        graph,
        lease_env(&authority, graph, "z2-a", &claim_a, "enforce", Some(0)),
    );
    let endpoint_a = endpoint(&profile_a);
    put(&endpoint_a, graph, "seed", "seed", "one").await;
    assert!(process::wait_for_current_seq(
        &durable,
        1,
        Duration::from_secs(20)
    ));
    authority.pause_renew("z2-a", true);
    assert!(authority.wait_until_expired(graph, Duration::from_secs(5)));

    let claim_b = authority.claim(graph, "z2-b", 1);
    let profile_b = root.child("profile-b");
    let (mut child_b, _) = spawn(
        &root,
        "z2-b",
        profile_b.clone(),
        durable.clone(),
        graph,
        lease_env(&authority, graph, "z2-b", &claim_b, "enforce", Some(1)),
    );
    let endpoint_b = endpoint(&profile_b);

    let started = Instant::now();
    let deadline = started + Duration::from_secs(60);
    let mut iteration = 0_u64;
    let mut last_observed = 1_u64;
    let mut observed = BTreeSet::from([1_u64]);
    while Instant::now() < deadline {
        iteration += 1;
        put(
            &endpoint_a,
            graph,
            "stale-writer",
            &format!("stale-{iteration}"),
            "never durable",
        )
        .await;
        put(
            &endpoint_b,
            graph,
            "live-writer",
            &format!("live-{iteration}"),
            &format!("iteration {iteration}"),
        )
        .await;

        let sample_until = (Instant::now() + Duration::from_millis(900)).min(deadline);
        while Instant::now() < sample_until {
            if let Some(seq) = process::durable_current_seq(&durable) {
                assert!(
                    seq >= last_observed,
                    "CURRENT regressed from {last_observed} to {seq}"
                );
                last_observed = seq;
                observed.insert(seq);
                let recorded = authority.events(graph).iter().any(|event| {
                    event.seq == Some(seq) && matches!(event.kind.as_str(), "intent" | "commit")
                });
                assert!(
                    recorded,
                    "observed CURRENT={seq} before any authority record"
                );
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    assert!(
        started.elapsed() >= Duration::from_secs(60),
        "Z2 must exercise the full 60-second window"
    );
    assert!(
        child_a.is_running(),
        "stale writer died during the 60s overlap"
    );
    assert!(
        child_b.is_running(),
        "live writer died during the 60s overlap"
    );
    assert!(observed.len() >= 2, "live writer did not advance CURRENT");

    assert!(process::wait_until(Duration::from_secs(10), || {
        let current = process::durable_current_seq(&durable);
        let row = authority.describe(graph).unwrap();
        current == Some(row.last_snap) && row.pending_snap.is_none()
    }));
    let committed: BTreeSet<u64> = authority
        .events(graph)
        .into_iter()
        .filter(|event| event.kind == "commit")
        .filter_map(|event| event.seq)
        .collect();
    for name in process::list_snap_dirs(&durable) {
        let seq = name.strip_prefix("snap-").unwrap().parse::<u64>().unwrap();
        assert!(
            committed.contains(&seq),
            "physical snapshot {name} has no committed authority record"
        );
    }

    authority.pause_renew("z2-a", false);
    let stale_status = process::wait_with_timeout(&mut child_a.child, Duration::from_secs(15))
        .expect("stale writer exits after renewal resumes");
    assert_eq!(stale_status.code(), Some(4));
    terminate_cleanly(&mut child_b);
}

#[tokio::test(flavor = "current_thread")]
async fn z3_authority_outage_fences_durability_but_keeps_reads_and_process_alive() {
    let _guard = process_test_lock();
    let root = ScratchDir::new("z3");
    let graph = "z3-outage";
    let durable = root.child("durable");
    let authority = LeaseAuthority::start(TOKEN, 1_500, 0);
    authority.seed_expired(graph, 0, 0);
    let claim = authority.claim(graph, "z3-a", 0);
    let profile = root.child("profile");
    process::prime_graph(&profile, graph);
    let mut env = lease_env(&authority, graph, "z3-a", &claim, "enforce", Some(0));
    env.insert("GARDEN_LEASE_MARGIN_MS".into(), "500".into());
    env.insert("GARDEN_LEASE_FENCED_MAX_MS".into(), "30000".into());
    let (mut child, logs) = spawn(&root, "z3", profile.clone(), durable.clone(), graph, env);
    let endpoint = endpoint(&profile);
    put(&endpoint, graph, "state", "before-outage", "durable").await;
    assert!(process::wait_for_current_seq(
        &durable,
        1,
        Duration::from_secs(20)
    ));

    authority.set_available(false);
    assert!(
        wait_for_health_status(&endpoint, "fenced", Duration::from_secs(5)).await,
        "health never became fenced"
    );
    let (health_status, health) = endpoint.health().await;
    assert_eq!(health_status, StatusCode::OK);
    assert_eq!(health["status"], "fenced");

    // OR-6: writes may be accepted into the local profile while fenced.
    put(
        &endpoint,
        graph,
        "state",
        "during-outage",
        "memory only until recovery",
    )
    .await;
    let readable = get(&endpoint, graph, "state").await;
    assert_eq!(readable["title"], "during-outage");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(child.is_running(), "fenced cell must remain alive");
    assert_eq!(process::durable_current_seq(&durable), Some(1));
    assert_eq!(authority.describe(graph).unwrap().last_snap, 1);
    assert!(process::wait_for_log(
        &logs,
        "z3",
        "stderr",
        "durable flush FENCED at Gate A",
        Duration::from_secs(5)
    ));

    authority.set_available(true);
    assert!(
        wait_for_health_status(&endpoint, "ok", Duration::from_secs(5)).await,
        "health never recovered"
    );
    assert!(
        process::wait_for_current_seq(&durable, 2, Duration::from_secs(20)),
        "pending dirty state was not published after un-fence"
    );
    assert_eq!(authority.describe(graph).unwrap().last_snap, 2);
    assert!(child.is_running());
    terminate_cleanly(&mut child);
}

#[tokio::test(flavor = "current_thread")]
async fn z6_sigterm_and_idle_publish_before_transactional_release() {
    let _guard = process_test_lock();
    for idle in [false, true] {
        let label = if idle { "idle" } else { "sigterm" };
        let root = ScratchDir::new(&format!("z6-{label}"));
        let graph = format!("z6-{label}");
        let durable = root.child("durable");
        let authority = LeaseAuthority::start(TOKEN, 20_000, 0);
        authority.seed_expired(&graph, 0, 0);
        let holder = format!("z6-{label}-holder");
        let claim = authority.claim(&graph, &holder, 0);
        let profile = root.child("profile");
        process::prime_graph(&profile, &graph);
        let mut env = lease_env(&authority, &graph, &holder, &claim, "enforce", Some(0));
        env.insert("GARDEN_FLUSH_DEBOUNCE_SECONDS".into(), "3600".into());
        env.insert("GARDEN_FLUSH_INTERVAL_SECONDS".into(), "3600".into());
        if idle {
            env.insert("GARDEN_IDLE_TTL_SECONDS".into(), "3".into());
        }
        let (mut child, _) = spawn(&root, label, profile.clone(), durable.clone(), &graph, env);
        let endpoint = endpoint(&profile);
        put(&endpoint, &graph, "state", label, "final flush").await;
        if !idle {
            process::send_sigterm(&child.child);
        }
        let status = process::wait_with_timeout(&mut child.child, Duration::from_secs(30))
            .expect("clean shutdown should finish");
        assert_eq!(status.code(), Some(0));
        assert_eq!(process::durable_current_seq(&durable), Some(1));
        let row = authority.describe(&graph).unwrap();
        assert_eq!(row.last_snap, 1);
        assert_eq!(row.holder, None);
        let events = authority.events(&graph);
        let commit = events
            .iter()
            .find(|event| event.kind == "commit" && event.seq == Some(1))
            .expect("commit event");
        let release = events
            .iter()
            .find(|event| event.kind == "release")
            .expect("release event");
        assert!(
            commit.id < release.id,
            "final publish must commit before release: {events:?}"
        );
        assert!(commit.at_ms <= release.at_ms);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn final_flush_gate_c_terminal_is_rechecked_before_release_or_exit_success() {
    let _guard = process_test_lock();
    let root = ScratchDir::new("final-gate-c");
    let graph = "final-gate-c";
    let durable = root.child("durable");
    let authority = LeaseAuthority::start(TOKEN, 20_000, 0);
    authority.seed_expired(graph, 0, 0);
    let claim = authority.claim(graph, "final-a", 0);
    let profile = root.child("profile");
    process::prime_graph(&profile, graph);
    let mut env = lease_env(&authority, graph, "final-a", &claim, "enforce", Some(0));
    env.insert("GARDEN_FLUSH_DEBOUNCE_SECONDS".into(), "3600".into());
    env.insert("GARDEN_FLUSH_INTERVAL_SECONDS".into(), "3600".into());
    let (mut child, logs) = spawn(
        &root,
        "final-a",
        profile.clone(),
        durable.clone(),
        graph,
        env,
    );
    let endpoint = endpoint(&profile);
    put(&endpoint, graph, "state", "final-only", "dirty").await;
    authority.omit_epoch_on_publish_conflict(true);
    authority.supersede_on_next_commit("final-a");
    process::send_sigterm(&child.child);

    let status = process::wait_with_timeout(&mut child.child, Duration::from_secs(30))
        .expect("Gate C terminal during final flush should exit");
    assert_eq!(status.code(), Some(4));
    let stderr = process::read_log(&logs, "final-a", "stderr");
    assert!(
        stderr.contains("terminal evidence arose during the final durable flush"),
        "missing post-flush lease recheck testimony:\n{stderr}"
    );
    assert!(
        !authority
            .events(graph)
            .iter()
            .any(|event| event.kind == "release"),
        "terminal final flush must never release the stale holder"
    );
    assert_eq!(authority.describe(graph).unwrap().last_snap, 0);
    assert_eq!(process::durable_current_seq(&durable), Some(1));
}

#[tokio::test(flavor = "current_thread")]
async fn stuck_flush_duration_reaches_authority_and_forfeits_without_expiry_extension() {
    let _guard = process_test_lock();
    let root = ScratchDir::new("stuck-flush");
    let graph = "stuck-flush";
    let durable = root.child("durable");
    // Keep the stuck threshold comfortably above an ordinary local flush,
    // then inject a much longer Gate-B response delay for the dirty flush.
    // This makes the forfeit evidence causal rather than scheduler-sensitive.
    let authority = LeaseAuthority::start_with_stuck_flush(TOKEN, 5_000, 0, 500);
    authority.seed_expired(graph, 0, 0);
    let claim = authority.claim(graph, "stuck-holder", 0);
    let profile = root.child("profile");
    process::prime_graph(&profile, graph);
    let mut env = lease_env(
        &authority,
        graph,
        "stuck-holder",
        &claim,
        "enforce",
        Some(0),
    );
    env.insert("GARDEN_LEASE_RENEW_MS".into(), "20".into());
    env.insert("GARDEN_LEASE_MARGIN_MS".into(), "200".into());
    env.insert("GARDEN_FLUSH_DEBOUNCE_SECONDS".into(), "2".into());
    env.insert("GARDEN_FLUSH_INTERVAL_SECONDS".into(), "2".into());
    let (mut child, logs) = spawn(&root, "stuck", profile.clone(), durable.clone(), graph, env);
    let endpoint = endpoint(&profile);
    assert!(process::wait_for_current_seq(
        &durable,
        1,
        Duration::from_secs(20)
    ));
    // CURRENT is deliberately rewritten before Gate C commits `last_snap`.
    // Wait for the initial authority commit before delaying the next publish,
    // otherwise this setup can race the real HTTP Gate C request.
    assert!(
        process::wait_until(Duration::from_secs(5), || {
            authority
                .describe(graph)
                .is_some_and(|row| row.last_snap == 1)
        }),
        "initial Gate C commit never reached the authority"
    );
    authority.set_publish_delay_ms(1_500);
    put(&endpoint, graph, "state", "slow-flush", "forfeit").await;
    let expiry_before_forfeit = authority.describe(graph).unwrap().expires_at;
    let status = process::wait_with_timeout(&mut child.child, Duration::from_secs(20))
        .expect("lease_forfeit should terminate the cell");
    assert_eq!(status.code(), Some(4));
    // The authority request deliberately outlives the process's terminal
    // selection. Give its real delayed Gate-B CAS time to linearize, then
    // prove the epoch bump rejected it before CURRENT could move.
    std::thread::sleep(Duration::from_millis(400));
    let row = authority.describe(graph).unwrap();
    assert_eq!(row.state, "active");
    assert_eq!(row.epoch, claim.epoch + 1);
    assert_eq!(row.holder, None);
    assert!(row.stuck_since.is_some());
    assert!(
        row.expires_at <= expiry_before_forfeit,
        "forfeit extended expiry: before={expiry_before_forfeit}, after={}",
        row.expires_at
    );
    assert_eq!(row.last_snap, 1);
    assert_eq!(row.pending_snap, None);
    assert_eq!(process::durable_current_seq(&durable), Some(1));
    assert!(
        authority.max_flush_in_progress_ms() > 500,
        "renew never observed the live RAII flush marker"
    );
    assert!(
        authority
            .events(graph)
            .iter()
            .any(|event| event.kind == "forfeit"),
        "missing transactional forfeit event"
    );
    assert!(
        !authority.events(graph).iter().any(|event| {
            event.seq == Some(2) && matches!(event.kind.as_str(), "intent" | "commit")
        }),
        "the invalidated in-flight flush reached the durable bridge"
    );
    assert!(process::read_log(&logs, "stuck", "stderr")
        .contains("skipping the final durable flush entirely"));
}

#[tokio::test(flavor = "current_thread")]
async fn enforce_rechecks_terminal_state_after_effective_delay_before_repair_or_serve() {
    let _guard = process_test_lock();
    let root = ScratchDir::new("boot-recheck");
    let graph = "boot-recheck";
    let durable = root.child("durable");
    let authority = LeaseAuthority::start(TOKEN, 400, 2_000);
    authority.seed_expired(graph, 0, 0);
    let claim_a = authority.claim(graph, "boot-a", 0);
    assert!(
        claim_a.effective_at_ms
            > std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as i64
    );
    let profile = root.child("profile");
    process::prime_graph(&profile, graph);
    let mut env = lease_env(&authority, graph, "boot-a", &claim_a, "enforce", Some(0));
    env.insert("GARDEN_LEASE_RENEW_MS".into(), "50".into());
    env.insert("GARDEN_LEASE_MARGIN_MS".into(), "100".into());
    let (mut child, logs) = spawn(
        &root,
        "boot-a",
        profile.clone(),
        durable.clone(),
        graph,
        env,
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    authority.pause_renew("boot-a", true);
    assert!(authority.wait_until_expired(graph, Duration::from_secs(3)));
    let claim_b = authority.claim(graph, "boot-b", 0);
    assert_eq!(claim_b.epoch, claim_a.epoch + 1);
    authority.pause_renew("boot-a", false);

    let status = process::wait_with_timeout(&mut child.child, Duration::from_secs(10))
        .expect("terminal lease during effective wait should refuse boot");
    assert_eq!(status.code(), Some(4));
    assert!(
        !profile.join("loopback.json").exists(),
        "loopback must never be served after terminal evidence during boot"
    );
    assert_eq!(fs::read_dir(&durable).unwrap().count(), 0);
    let stderr = process::read_log(&logs, "boot-a", "stderr");
    assert!(
        stderr.contains("became terminal during the effective-delay wait")
            || stderr.contains("write lease became terminal after hydrate")
            || stderr.contains("write lease terminated before ever renewing successfully"),
        "missing enforce boot-boundary terminal refusal:\n{stderr}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn enforce_rechecks_terminal_state_after_hydrate_before_setup_or_boot_ready() {
    let _guard = process_test_lock();
    let root = ScratchDir::new("post-hydrate-recheck");
    let graph = "post-hydrate-recheck";
    let holder = "01J0000000000000000000000H";
    let durable = root.child("durable");
    let authority = LeaseAuthority::start(TOKEN, 20_000, 0);
    authority.seed_expired(graph, 0, 0);
    let claim = authority.claim(graph, holder, 0);
    let profile = root.child("profile");
    process::prime_graph(&profile, graph);
    let mut env = lease_env(&authority, graph, holder, &claim, "enforce", Some(0));
    env.insert("GARDEN_LEASE_RENEW_MS".into(), "50".into());
    env.insert(
        "GARDEN_LEASE_TEST_POST_HYDRATE_DELAY_MS".into(),
        "1000".into(),
    );
    // Enable real CaptureEvent stdout so the regression proves that this
    // terminal boot emits failure testimony, never a false ready event.
    env.insert("SOPHIA_OBSERVATORY_CAPTURE_ENABLED".into(), "true".into());
    env.insert(
        "SOPHIA_OBSERVATORY_CONTRACT_BUNDLE_SHA256".into(),
        "c60d81fe4b431c3a88bd2450189a16da97627cc6ba36c4b127942d593a6c2bbb".into(),
    );
    env.insert("GARDEN_CELL_MACHINE_ID".into(), format!("cell:{graph}"));
    let (mut child, logs) = spawn(&root, "post-hydrate", profile.clone(), durable, graph, env);
    assert!(
        process::wait_for_log(
            &logs,
            "post-hydrate",
            "stderr",
            "TEST ONLY: delaying",
            Duration::from_secs(20),
        ),
        "process never reached the deterministic post-hydrate window:\n{}",
        process::read_log(&logs, "post-hydrate", "stderr")
    );
    authority.retire(graph);

    let status = process::wait_with_timeout(&mut child.child, Duration::from_secs(10))
        .expect("terminal lease after hydrate should refuse setup");
    assert_eq!(status.code(), Some(4));
    assert!(
        !profile.join("loopback.json").exists(),
        "loopback API must never be exposed after terminal evidence during boot"
    );
    let stderr = process::read_log(&logs, "post-hydrate", "stderr");
    assert!(
        stderr.contains("after hydrate, before core setup"),
        "missing post-hydrate guard testimony:\n{stderr}"
    );
    let stdout = process::read_log(&logs, "post-hydrate", "stdout");
    let events = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .collect::<Vec<_>>();
    assert!(
        events.iter().any(|event| {
            event["kind"] == "cell.boot"
                && event["outcome"] == "error"
                && event["payload"]["failure_stage"] == "lease_boot"
        }),
        "missing lease_boot failure testimony: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| { event["kind"] == "cell.boot" && event["payload"]["stage"] == "ready" }),
        "terminal boot emitted false ready testimony: {events:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn enforce_integrates_terminal_check_into_loopback_exposure_gate() {
    let _guard = process_test_lock();
    let root = ScratchDir::new("pre-loopback-recheck");
    let graph = "pre-loopback-recheck";
    let holder = "01J0000000000000000000000K";
    let durable = root.child("durable");
    let authority = LeaseAuthority::start(TOKEN, 20_000, 0);
    authority.seed_expired(graph, 0, 0);
    let claim = authority.claim(graph, holder, 0);
    let profile = root.child("profile");
    process::prime_graph(&profile, graph);
    let mut env = lease_env(&authority, graph, holder, &claim, "enforce", Some(0));
    env.insert("GARDEN_LEASE_RENEW_MS".into(), "50".into());
    env.insert(
        "GARDEN_LEASE_TEST_PRE_LOOPBACK_DELAY_MS".into(),
        "1000".into(),
    );
    env.insert("SOPHIA_OBSERVATORY_CAPTURE_ENABLED".into(), "true".into());
    env.insert(
        "SOPHIA_OBSERVATORY_CONTRACT_BUNDLE_SHA256".into(),
        "c60d81fe4b431c3a88bd2450189a16da97627cc6ba36c4b127942d593a6c2bbb".into(),
    );
    env.insert("GARDEN_CELL_MACHINE_ID".into(), format!("cell:{graph}"));
    let (mut child, logs) = spawn(&root, "pre-loopback", profile.clone(), durable, graph, env);
    assert!(
        process::wait_for_log(
            &logs,
            "pre-loopback",
            "stderr",
            "TEST ONLY: delaying",
            Duration::from_secs(20),
        ),
        "process never reached the deterministic service-exposure gate:\n{}",
        process::read_log(&logs, "pre-loopback", "stderr")
    );
    authority.retire(graph);

    let status = process::wait_with_timeout(&mut child.child, Duration::from_secs(10))
        .expect("terminal lease at service-exposure gate should refuse boot");
    assert_eq!(status.code(), Some(4));
    assert!(
        !profile.join("loopback.json").exists(),
        "terminal service-exposure gate must never bind/write a loopback manifest"
    );
    let stderr = process::read_log(&logs, "pre-loopback", "stderr");
    assert!(
        stderr.contains("while waiting for API readiness"),
        "wait-ready failure did not route through the lease guard:\n{stderr}"
    );
    assert!(
        stderr.contains("before loopback exposure"),
        "missing integrated service-exposure refusal:\n{stderr}"
    );
    let events = process::read_log(&logs, "pre-loopback", "stdout")
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .collect::<Vec<_>>();
    assert!(
        events.iter().any(|event| {
            event["kind"] == "cell.boot"
                && event["outcome"] == "error"
                && event["payload"]["failure_stage"] == "lease_boot"
        }),
        "missing lease_boot failure testimony: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| { event["kind"] == "cell.boot" && event["payload"]["stage"] == "ready" }),
        "terminal service-exposure race emitted false ready testimony: {events:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn terminal_during_bound_but_not_ready_window_refuses_every_api_request() {
    let _guard = process_test_lock();
    let root = ScratchDir::new("bound-not-ready-terminal");
    let graph = "bound-not-ready-terminal";
    let holder = "01J0000000000000000000000B";
    let durable = root.child("durable");
    let authority = LeaseAuthority::start(TOKEN, 20_000, 0);
    authority.seed_expired(graph, 0, 0);
    let claim = authority.claim(graph, holder, 0);
    let profile = root.child("profile");
    process::prime_graph(&profile, graph);
    let mut env = lease_env(&authority, graph, holder, &claim, "enforce", Some(0));
    env.insert("GARDEN_LEASE_RENEW_MS".into(), "50".into());
    env.insert(
        "GARDEN_LEASE_TEST_POST_LOOPBACK_PRE_READY_DELAY_MS".into(),
        "1500".into(),
    );
    env.insert("SOPHIA_OBSERVATORY_CAPTURE_ENABLED".into(), "true".into());
    env.insert(
        "SOPHIA_OBSERVATORY_CONTRACT_BUNDLE_SHA256".into(),
        "c60d81fe4b431c3a88bd2450189a16da97627cc6ba36c4b127942d593a6c2bbb".into(),
    );
    env.insert("GARDEN_CELL_MACHINE_ID".into(), format!("cell:{graph}"));
    let (mut child, logs) = spawn(
        &root,
        "bound-not-ready",
        profile.clone(),
        durable,
        graph,
        env,
    );
    assert!(process::wait_for_log(
        &logs,
        "bound-not-ready",
        "stderr",
        "after loopback bind before readiness completion",
        Duration::from_secs(20),
    ));
    let bound_endpoint = endpoint(&profile);
    authority.retire(graph);
    assert!(
        wait_for_health_status(&bound_endpoint, "lease_terminal", Duration::from_secs(5),).await,
        "terminal bound socket continued to advertise readiness"
    );
    let (health_status, health) = bound_endpoint.health().await;
    assert_eq!(health_status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(health["status"], "lease_terminal");
    let (put_status, put_body) = bound_endpoint
        .put_document(graph, "forbidden", "must-not-land", "terminal")
        .await;
    assert_eq!(put_status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(put_body["error"], "lease_terminal");

    let status = process::wait_with_timeout(&mut child.child, Duration::from_secs(10))
        .expect("post-readiness lease guard should terminate the cell");
    assert_eq!(status.code(), Some(4));
    let events = process::read_log(&logs, "bound-not-ready", "stdout")
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .collect::<Vec<_>>();
    assert!(
        !events
            .iter()
            .any(|event| { event["kind"] == "cell.boot" && event["payload"]["stage"] == "ready" }),
        "terminal bound socket emitted false ready testimony: {events:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn observe_mode_409_testifies_but_never_fences_or_exits() {
    let _guard = process_test_lock();
    let root = ScratchDir::new("observe-409");
    let graph = "observe-409";
    let durable = root.child("durable");
    let authority = LeaseAuthority::start(TOKEN, 20_000, 0);
    authority.seed_expired(graph, 0, 0);
    let holder = "01J0000000000000000000000R";
    let stale_claim = authority.claim(graph, holder, 0);
    // Replace the row before the process starts so every real renew/publish
    // request receives the same 409 shape a successor would cause.
    authority.seed_expired(graph, stale_claim.epoch + 1, 0);
    let profile = root.child("profile");
    process::prime_graph(&profile, graph);
    let mut env = lease_env(&authority, graph, holder, &stale_claim, "observe", Some(0));
    env.extend([
        ("SOPHIA_OBSERVATORY_CAPTURE_ENABLED".into(), "true".into()),
        (
            "SOPHIA_OBSERVATORY_CONTRACT_BUNDLE_SHA256".into(),
            "c60d81fe4b431c3a88bd2450189a16da97627cc6ba36c4b127942d593a6c2bbb".into(),
        ),
        ("GARDEN_CELL_MACHINE_ID".into(), format!("cell:{graph}")),
    ]);
    let (mut child, logs) = spawn(
        &root,
        "observe",
        profile.clone(),
        durable.clone(),
        graph,
        env,
    );
    let endpoint = endpoint(&profile);
    let (status, health) = endpoint.health().await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(health["status"], "fenced");
    put(&endpoint, graph, "state", "observe-write", "availability").await;
    assert!(process::wait_for_current_seq(
        &durable,
        1,
        Duration::from_secs(20)
    ));
    assert!(child.is_running());
    assert!(process::wait_for_log(
        &logs,
        "observe",
        "stderr",
        "WOULD HAVE terminated in observe mode",
        Duration::from_secs(5)
    ));
    assert!(
        process::read_log(&logs, "observe", "stderr").contains("WOULD HAVE been fenced at Gate B")
    );
    terminate_cleanly(&mut child);
    let events = process::read_log(&logs, "observe", "stdout")
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(events.iter().any(|event| {
        event["kind"] == "dep.state"
            && event["payload"]["dependency"] == "write_lease"
            && event["payload"]["state"] == "degraded"
            && event["payload"]["detail_code"] == "lease_renew_lost"
    }));
    assert!(events.iter().any(|event| {
        event["kind"] == "dep.state"
            && event["payload"]["dependency"] == "write_lease"
            && event["payload"]["state"] == "degraded"
            && event["payload"]["detail_code"] == "lease_gate_b_publish_intent"
    }));
}

#[test]
fn z7_legacy_boot_runs_unfenced_with_a_warning() {
    let _guard = process_test_lock();
    let root = ScratchDir::new("z7-legacy");
    let profile = root.child("profile");
    let durable = root.child("durable");
    process::prime_graph(&profile, "z7");
    let (mut child, log_dir) = spawn(
        &root,
        "legacy",
        profile,
        durable,
        "z7",
        [
            ("GARDEN_IDLE_TTL_SECONDS".into(), "3".into()),
            ("RUST_LOG".into(), "info".into()),
        ]
        .into_iter()
        .collect(),
    );
    let status = process::wait_with_timeout(&mut child.child, Duration::from_secs(30))
        .expect("legacy gardend should idle-reap");
    assert_eq!(status.code(), Some(0));
    let stderr = process::read_log(&log_dir, "legacy", "stderr");
    assert!(
        stderr.contains("UNFENCED"),
        "expected the literal legacy warning, got:\n{stderr}"
    );
}

#[test]
fn z7_enforce_missing_epoch_refuses_boot() {
    let _guard = process_test_lock();
    let root = ScratchDir::new("z7-enforce");
    let profile = root.child("profile");
    let durable = root.child("durable");
    process::prime_graph(&profile, "z7");
    let (mut child, _) = spawn(
        &root,
        "enforce",
        profile,
        durable.clone(),
        "z7",
        [
            ("GARDEN_LEASE_MODE".into(), "enforce".into()),
            ("RUST_LOG".into(), "info".into()),
        ]
        .into_iter()
        .collect(),
    );
    let status = process::wait_with_timeout(&mut child.child, Duration::from_secs(30))
        .expect("enforce refusal should exit");
    assert_eq!(status.code(), Some(4));
    assert_eq!(fs::read_dir(durable).unwrap().count(), 0);
}
