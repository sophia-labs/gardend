//! `gardend index --graph <id>`: turn on `find` for a graph and build its RDF
//! entity index (`entity_index.rs`), or turn it off.
//!
//! - No gardend serving the profile: the index is built here, now.
//! - A gardend serving it: the setting is written, then that gardend builds
//!   the index in a semantic refresh job (the store takes one process); this
//!   command follows the job until it ends.

use crate::entity_index::{
    read_entity_index_config, refresh_entity_index, write_entity_index_config, EntityIndexConfig,
    RefreshOptions,
};
use oxigraph::store::Store;
use std::path::Path;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexArgs {
    pub profile: Option<String>,
    pub graph: String,
    /// `--off`: turn `find` off for the graph (the files stay).
    pub off: bool,
    /// `--langs es,en`: label and description preference.
    pub langs: Option<Vec<String>>,
    pub model: Option<String>,
    /// `--documents`: index the graph's documents too.
    pub documents: bool,
    pub threads: Option<usize>,
}

pub fn index(profile_dir: &Path, args: &IndexArgs) -> Result<String, String> {
    crate::ids::validate_local_id(&args.graph, "--graph")?;
    let graph_dir = profile_dir.join("graphs").join(&args.graph);
    if !graph_dir.join("graph.json").is_file() {
        return Err(format!("no graph {:?} in {}", args.graph, profile_dir.display()));
    }
    let mut config = read_entity_index_config(&graph_dir).unwrap_or(EntityIndexConfig {
        enabled: false,
        model: String::new(),
        langs: Vec::new(),
        documents: false,
    });
    if args.off {
        config.enabled = false;
        write_entity_index_config(&graph_dir, &config)?;
        return Ok(format!(
            "find is off for graph {} (its index files stay in indexes/entities/; `gardend index --graph {}` turns it back on)\n",
            args.graph, args.graph
        ));
    }
    config.enabled = true;
    if let Some(langs) = &args.langs {
        config.langs = langs.clone();
    }
    if let Some(model) = &args.model {
        config.model = model.clone();
    }
    if args.documents {
        config.documents = true;
    }
    write_entity_index_config(&graph_dir, &config)?;
    match super::serving_now(profile_dir) {
        Some(running) => build_on_server(&running, &args.graph),
        None => build_here(profile_dir, &args.graph, args.threads).map(|summary| format!("{summary}\n")),
    }
}

/// After `gardend load`, on a graph with `find` on: index what was loaded.
pub(crate) fn refresh_after_load(profile_dir: &Path, graph: &str, threads: Option<usize>) -> Option<Result<String, String>> {
    let graph_dir = profile_dir.join("graphs").join(graph);
    if !crate::entity_index::entity_index_enabled(&graph_dir) {
        return None;
    }
    Some(build_here(profile_dir, graph, threads))
}

fn build_here(profile_dir: &Path, graph: &str, threads: Option<usize>) -> Result<String, String> {
    let graph_dir = profile_dir.join("graphs").join(graph);
    let store_path = graph_dir.join("store.oxigraph");
    let store = Store::open(&store_path).map_err(|error| {
        format!("open the store of {graph} ({}): {error}. Is a gardend still running on this profile?", store_path.display())
    })?;
    let mut last = Instant::now() - Duration::from_secs(60);
    let report = refresh_entity_index(
        graph,
        &graph_dir,
        &store,
        &RefreshOptions { threads, batch: None },
        &mut |stage, done, total| {
            if last.elapsed() >= Duration::from_secs(5) || (total > 0 && done == total) {
                last = Instant::now();
                if total > 0 {
                    eprintln!("  {stage}: {done}/{total}");
                } else {
                    eprintln!("  {stage}");
                }
            }
            Ok(())
        },
    )?;
    Ok(report.summary())
}

fn build_on_server(running: &super::RunningManifest, graph: &str) -> Result<String, String> {
    let base = format!("http://127.0.0.1:{}", running.port);
    let agent = ureq::Agent::new_with_defaults();
    let auth = format!("Bearer {}", running.token);
    let submitted: serde_json::Value = agent
        .post(&format!("{base}/api/semantic/index/refresh/jobs"))
        .header("Authorization", &auth)
        .send_json(serde_json::json!({ "graphId": graph, "entitiesOnly": true }))
        .map_err(|error| format!("ask the running gardend (pid {}) to index {graph}: {error}", running.pid))?
        .body_mut()
        .read_json()
        .map_err(|error| format!("the running gardend's answer: {error}"))?;
    let job_id = submitted["job_id"]
        .as_str()
        .ok_or_else(|| format!("the running gardend started no job: {submitted}"))?
        .to_string();
    eprintln!("gardend (pid {}) is serving this profile: indexing {graph} there, job {job_id}", running.pid);
    let mut last_phase = String::new();
    loop {
        std::thread::sleep(Duration::from_secs(2));
        let status: serde_json::Value = agent
            .get(&format!("{base}/api/semantic/index/refresh/jobs/{job_id}"))
            .header("Authorization", &auth)
            .call()
            .map_err(|error| format!("job {job_id}: {error}"))?
            .body_mut()
            .read_json()
            .map_err(|error| format!("job {job_id}: {error}"))?;
        let progress = &status["progress"];
        let phase = format!(
            "{} {}/{}",
            progress["phase"].as_str().unwrap_or(""),
            progress["current"].as_u64().unwrap_or(0),
            progress["total"].as_u64().unwrap_or(0)
        );
        if phase != last_phase {
            eprintln!("  {phase}");
            last_phase = phase;
        }
        match status["status"].as_str().unwrap_or("") {
            "succeeded" => {
                let mut report = progress["details"]["result"]["entityIndex"].clone();
                if report.get("entities").is_none() {
                    // the job's result holds it when the last progress report does not
                    if let Ok(mut response) = agent
                        .get(&format!("{base}/graphs/jobs/{job_id}/result"))
                        .header("Authorization", &auth)
                        .call()
                    {
                        if let Ok(result) = response.body_mut().read_json::<serde_json::Value>() {
                            report = result
                                .get("entityIndex")
                                .or_else(|| result.pointer("/result/entityIndex"))
                                .cloned()
                                .unwrap_or_default();
                        }
                    }
                }
                return Ok(match report.get("entities") {
                    Some(_) => format!(
                        "entity index {graph}: {} entities ({} embedded, {} reused), {:.1} s\n",
                        report["entities"], report["embedded"], report["reused"],
                        report["seconds"].as_f64().unwrap_or(0.0)
                    ),
                    None => format!("job {job_id} succeeded\n"),
                });
            }
            "failed" | "cancelled" => {
                return Err(format!(
                    "job {job_id} {}: {}",
                    status["status"].as_str().unwrap_or(""),
                    status["error"].as_str().unwrap_or("(no error)")
                ))
            }
            _ => {}
        }
    }
}
