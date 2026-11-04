//! Append microbenchmark. Prints one JSON line:
//! {appends, append_p50_us, append_p99_us, barrier_p50_us, full_p50_us, bytes_per_event}.

use std::time::Instant;

use garden_ledger::{Draft, HlcClock, Ledger, Mode, OriginId, Subject};

fn draft(payload: &[u8], i: usize) -> Draft {
    Draft {
        witness: "urn:bench:witness".to_string(),
        vocab: "urn:bench:vocab".to_string(),
        subjects: vec![Subject {
            subject: format!("urn:bench:s{}", i % 1024),
            class: "urn:bench:Thing".to_string(),
            parents: Vec::new(),
        }],
        mode: Mode::Assert,
        payload: payload.to_vec(),
        txn: None,
    }
}

fn pct(sorted: &[u128], p: f64) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn main() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (mut ledger, _) =
        Ledger::open(dir.path(), OriginId("bench".to_string())).expect("open ledger");
    let mut clock = HlcClock::new();
    let payload = vec![0x61u8; 1024];
    let mut wall = 1_700_000_000_000u64;

    const N: usize = 100_000;
    let mut lat: Vec<u128> = Vec::with_capacity(N);
    for i in 0..N {
        wall += 1;
        let d = draft(&payload, i);
        let t = Instant::now();
        ledger.append(&mut clock, wall, d).expect("append");
        lat.push(t.elapsed().as_micros());
    }
    lat.sort_unstable();
    let (p50, p99) = (pct(&lat, 0.50), pct(&lat, 0.99));

    // Bytes on disk per event, measured before the sync phases add more.
    let mut total: u64 = 0;
    for e in std::fs::read_dir(dir.path()).expect("read_dir") {
        total += e.expect("entry").metadata().expect("meta").len();
    }
    let bytes_per_event = total / N as u64;

    ledger.sync_full().expect("sync");

    let mut barrier: Vec<u128> = Vec::with_capacity(1000);
    for i in 0..1000 {
        wall += 1;
        let d = draft(&payload, i);
        let t = Instant::now();
        ledger.append(&mut clock, wall, d).expect("append");
        ledger.sync_barrier().expect("barrier");
        barrier.push(t.elapsed().as_micros());
    }
    barrier.sort_unstable();

    let mut full: Vec<u128> = Vec::with_capacity(100);
    for i in 0..100 {
        wall += 1;
        let d = draft(&payload, i);
        let t = Instant::now();
        ledger.append(&mut clock, wall, d).expect("append");
        ledger.sync_full().expect("full");
        full.push(t.elapsed().as_micros());
    }
    full.sort_unstable();

    println!(
        "{{\"appends\":{},\"append_p50_us\":{},\"append_p99_us\":{},\"barrier_p50_us\":{},\"full_p50_us\":{},\"bytes_per_event\":{}}}",
        N,
        p50,
        p99,
        pct(&barrier, 0.50),
        pct(&full, 0.50),
        bytes_per_event
    );
}
