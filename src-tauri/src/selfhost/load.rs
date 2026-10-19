//! `gardend load`: stream an RDF file into one graph of a profile through
//! Oxigraph's bulk loader.
//!
//! The HTTP import (`/graphs/{id}/imports/rdf`, `rdf_load`) reads its whole
//! body into memory and writes it in one transaction, capped at 500 MiB: fine
//! for documents' worth of RDF, not for a statistical cube of tens of millions
//! of quads. This verb never holds the file: it parses it as a stream (gzip
//! included) and hands the quads to the bulk loader, which sorts them into
//! RocksDB's own files and ingests them all at once on commit. A load that
//! fails (a parse error, a reserved graph, a full disk) commits nothing.
//!
//! It runs on a profile no gardend is serving (and is refused otherwise):
//! the store takes one process, and readers never see a half-loaded graph.
//! Where the quads go:
//! - triple formats (N-Triples, Turtle): the graph's `user:rdf` named graph,
//!   or `--into <iri>`;
//! - dataset formats (N-Quads, TriG): each quad's own named graph; a quad in
//!   the default graph goes to `user:rdf` (or `--into`). A quad naming one of
//!   the graph's reserved projection graphs (or the profile's) refuses the
//!   whole load, as the dataset import does.

use crate::rdf_authority::{is_reserved_rdf_graph_iri, user_rdf_graph_iri};
use oxigraph::io::{RdfFormat, RdfParser};
use oxigraph::model::{GraphName, NamedNode, Quad};
use oxigraph::store::Store;
use std::collections::BTreeMap;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoadArgs {
    pub profile: Option<String>,
    pub graph: String,
    /// `nt`, `nq`, `ttl` or `trig` (any name `parse_rdf_format` takes); else
    /// from the file name.
    pub format: Option<String>,
    /// The named graph for triples and default-graph quads.
    pub into: Option<String>,
    /// A file path, or `-` for standard input.
    pub input: String,
    /// Bulk-loader threads (default: up to 4).
    pub threads: Option<usize>,
    /// A rough ceiling on the loader's buffers, in megabytes.
    pub max_memory_mb: Option<usize>,
    /// `--no-index`: skip refreshing the graph's entity index (`find`) after
    /// the load, on a graph where it is on.
    pub no_index: bool,
}

/// What a load did, for the person and for tests.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadReport {
    pub graph_id: String,
    pub quads: u64,
    /// Quads per named graph they went into.
    pub graphs: BTreeMap<String, u64>,
    pub seconds: f64,
}

impl LoadReport {
    pub fn summary(&self) -> String {
        let rate = if self.seconds > 0.0 {
            self.quads as f64 / self.seconds
        } else {
            0.0
        };
        let mut text = format!(
            "loaded {} quads into graph {} in {:.1} s ({:.0} quads/s)\n",
            self.quads, self.graph_id, self.seconds, rate
        );
        for (graph, count) in &self.graphs {
            text.push_str(&format!("  {count:>12}  {graph}\n"));
        }
        text
    }
}

/// `gardend load`, from the CLI: refuse a served profile, load, report.
pub fn load(profile_dir: &Path, args: &LoadArgs) -> Result<String, String> {
    if let Some(running) = super::serving_now(profile_dir) {
        return Err(format!(
            "a gardend (pid {}) is serving {}: stop it first. gardend load writes the \
             graph's store directly, and only one process may hold a store (readers must \
             never see a half-loaded graph). Loading into a running gardend: rdf_load or \
             POST /graphs/{}/imports/rdf, up to 500 MiB.",
            running.pid,
            profile_dir.display(),
            args.graph
        ));
    }
    let report = load_into_profile(profile_dir, args, &mut |line| eprintln!("{line}"))?;
    let mut text = report.summary();
    // `find` on: what was loaded is indexed now (only changed entities embed)
    if !args.no_index {
        match super::index::refresh_after_load(profile_dir, &args.graph, args.threads) {
            Some(Ok(summary)) => text.push_str(&format!("{summary}\n")),
            Some(Err(error)) => {
                return Err(format!(
                    "{text}the load committed, but the entity index failed: {error}\n(retry with `gardend index --graph {}`)",
                    args.graph
                ))
            }
            None => {}
        }
    }
    Ok(text)
}

/// The load itself, with progress lines handed to `progress`. Does not check
/// whether a gardend serves the profile (`load` does); the store's own lock
/// refuses a second process anyway.
pub fn load_into_profile(
    profile_dir: &Path,
    args: &LoadArgs,
    progress: &mut dyn FnMut(String),
) -> Result<LoadReport, String> {
    crate::ids::validate_local_id(&args.graph, "--graph")?;
    let graph_dir = profile_dir.join("graphs").join(&args.graph);
    if !graph_dir.join("graph.json").is_file() {
        return Err(format!(
            "no graph {:?} in {}: create it first (create_graph over MCP, or the app), \
             then load",
            args.graph,
            profile_dir.display()
        ));
    }
    let format = resolve_format(args)?;
    let default_target = match args
        .into
        .as_deref()
        .map(str::trim)
        .filter(|iri| !iri.is_empty())
    {
        Some(iri) => {
            if is_reserved_rdf_graph_iri(&args.graph, iri) {
                return Err(format!("--into {iri} is a reserved projection graph"));
            }
            NamedNode::new(iri).map_err(|error| format!("--into {iri:?}: {error}"))?
        }
        None => NamedNode::new(user_rdf_graph_iri(&args.graph))
            .map_err(|error| format!("user:rdf graph IRI: {error}"))?,
    };
    let reader = open_input(&args.input)?;

    let store_path = graph_dir.join("store.oxigraph");
    let store = Store::open(&store_path).map_err(|error| {
        format!(
            "open the store of {} ({}): {error}. Is a gardend still running on this profile?",
            args.graph,
            store_path.display()
        )
    })?;

    let started = Instant::now();
    let loaded = Arc::new(AtomicU64::new(0));
    let threads = args
        .threads
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
                .min(4)
        })
        .max(1);
    let mut loader = store.bulk_loader().with_num_threads(threads);
    if let Some(megabytes) = args.max_memory_mb {
        loader = loader.with_max_memory_size_in_megabytes(megabytes.max(64));
    }
    {
        let loaded = Arc::clone(&loaded);
        loader = loader.on_progress(move |count| loaded.store(count, Ordering::Relaxed));
    }

    let graph_id = args.graph.clone();
    let mut per_graph: BTreeMap<String, u64> = BTreeMap::new();
    let mut parsed: u64 = 0;
    let mut last_report = Instant::now();
    let quads = RdfParser::from_format(format)
        .for_reader(reader)
        .map(|quad| -> Result<Quad, LoadError> {
            let mut quad = quad.map_err(|error| LoadError(format!("parse: {error}")))?;
            match &quad.graph_name {
                GraphName::DefaultGraph => {
                    quad.graph_name = GraphName::NamedNode(default_target.clone());
                }
                GraphName::NamedNode(named) => {
                    if is_reserved_rdf_graph_iri(&graph_id, named.as_str()) {
                        return Err(LoadError(format!(
                            "the file names the reserved projection graph {}",
                            named.as_str()
                        )));
                    }
                }
                GraphName::BlankNode(_) => {}
            }
            Ok(quad)
        })
        .inspect(|quad| {
            if let Ok(quad) = quad {
                parsed += 1;
                let key = match &quad.graph_name {
                    GraphName::NamedNode(named) => named.as_str().to_string(),
                    GraphName::BlankNode(blank) => format!("_:{}", blank.as_str()),
                    GraphName::DefaultGraph => String::new(),
                };
                *per_graph.entry(key).or_default() += 1;
                if last_report.elapsed() >= Duration::from_secs(5) {
                    last_report = Instant::now();
                    let seconds = started.elapsed().as_secs_f64();
                    progress(format!(
                        "  {parsed} quads read, {} sorted into the store ({:.0}/s)",
                        loaded.load(Ordering::Relaxed),
                        parsed as f64 / seconds.max(0.001)
                    ));
                }
            }
        });
    loader
        .load_ok_quads::<LoadError, LoadError>(quads)
        .map_err(|error| format!("{}; nothing was loaded", error.0))?;
    progress(format!("  {parsed} quads read; committing"));
    loader
        .commit()
        .map_err(|error| format!("commit the load: {error}; nothing was loaded"))?;
    drop(store);
    crate::graph_record_store::touch_graph_updated_at(&graph_dir).map_err(|error| {
        format!(
            "loaded, but touching graph.json failed: {}",
            error.message()
        )
    })?;
    Ok(LoadReport {
        graph_id: args.graph.clone(),
        quads: parsed,
        graphs: per_graph,
        seconds: started.elapsed().as_secs_f64(),
    })
}

#[derive(Debug)]
struct LoadError(String);

impl From<oxigraph::store::StorageError> for LoadError {
    fn from(error: oxigraph::store::StorageError) -> Self {
        Self(format!("store: {error}"))
    }
}

fn resolve_format(args: &LoadArgs) -> Result<RdfFormat, String> {
    if let Some(name) = args.format.as_deref() {
        return crate::rdf::parse_rdf_format(name);
    }
    if args.input == "-" {
        return Err("reading standard input needs --format nt|nq|ttl|trig".to_string());
    }
    let name = args.input.to_ascii_lowercase();
    let name = name.strip_suffix(".gz").unwrap_or(&name);
    let extension = Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default();
    match extension {
        "nt" | "nq" | "ttl" | "trig" => crate::rdf::parse_rdf_format(extension),
        _ => Err(format!(
            "cannot tell the format of {:?}: name it .nt, .nq, .ttl or .trig (optionally \
             .gz), or pass --format",
            args.input
        )),
    }
}

/// The input as a buffered stream; `.gz` files are decompressed on the fly.
fn open_input(input: &str) -> Result<Box<dyn Read>, String> {
    if input == "-" {
        return Ok(Box::new(BufReader::with_capacity(
            1 << 20,
            std::io::stdin(),
        )));
    }
    let path = PathBuf::from(input);
    let file =
        std::fs::File::open(&path).map_err(|error| format!("open {}: {error}", path.display()))?;
    let reader = BufReader::with_capacity(1 << 20, file);
    if input.to_ascii_lowercase().ends_with(".gz") {
        Ok(Box::new(BufReader::with_capacity(
            1 << 20,
            flate2::read::MultiGzDecoder::new(reader),
        )))
    } else {
        Ok(Box::new(reader) as Box<dyn Read>)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const GRAPH: &str = "cows";

    /// A scratch profile folder, removed when dropped.
    struct Scratch(PathBuf);

    impl Scratch {
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A profile with one graph, as gardend leaves it (`graph.json` and no
    /// served store open).
    fn profile_with_graph() -> Scratch {
        let profile =
            Scratch(std::env::temp_dir().join(format!("gardend-load-{}", uuid::Uuid::new_v4())));
        let graph_dir = profile.path().join("graphs").join(GRAPH);
        std::fs::create_dir_all(&graph_dir).unwrap();
        std::fs::write(
            graph_dir.join("graph.json"),
            serde_json::json!({
                "graphId": GRAPH,
                "title": "Cows",
                "origin": "local",
                "providerId": "local",
                "localPath": "",
                "createdAt": "1",
                "updatedAt": "1",
                "capabilities": []
            })
            .to_string(),
        )
        .unwrap();
        profile
    }

    fn args(input: &Path) -> LoadArgs {
        LoadArgs {
            graph: GRAPH.to_string(),
            input: input.display().to_string(),
            threads: Some(2),
            ..LoadArgs::default()
        }
    }

    fn count(profile: &Path, query: &str) -> u64 {
        let store = Store::open(profile.join("graphs").join(GRAPH).join("store.oxigraph")).unwrap();
        let oxigraph::sparql::QueryResults::Solutions(mut rows) =
            oxigraph::sparql::SparqlEvaluator::new()
                .parse_query(query)
                .unwrap()
                .on_store(&store)
                .execute()
                .unwrap()
        else {
            panic!("not a SELECT");
        };
        let row = rows.next().unwrap().unwrap();
        match row.get("n") {
            Some(oxigraph::model::Term::Literal(literal)) => literal.value().parse().unwrap(),
            other => panic!("no count: {other:?}"),
        }
    }

    /// A real file, gzipped, of quads in their own graphs and the default
    /// graph: loaded, then queried back by graph.
    #[test]
    fn a_gzipped_nquads_file_loads_into_its_graphs_and_queries_back() {
        let profile = profile_with_graph();
        let file = profile.path().join("cube.nq.gz");
        let mut encoder = flate2::write::GzEncoder::new(
            std::fs::File::create(&file).unwrap(),
            flate2::Compression::fast(),
        );
        for i in 0..25_000 {
            writeln!(
                encoder,
                "<urn:cow:obs:{i}> <urn:cow:value> \"{i}\"^^<http://www.w3.org/2001/XMLSchema#integer> <urn:cow:observations:2025> ."
            )
            .unwrap();
        }
        writeln!(encoder, "<urn:cow:cube> <urn:cow:label> \"the cube\" .").unwrap();
        encoder.finish().unwrap();

        let report = load_into_profile(profile.path(), &args(&file), &mut |_| {}).unwrap();
        assert_eq!(report.quads, 25_001);
        assert_eq!(report.graphs["urn:cow:observations:2025"], 25_000);
        let user_rdf = user_rdf_graph_iri(GRAPH);
        assert_eq!(report.graphs[&user_rdf], 1);
        assert_eq!(
            count(
                profile.path(),
                "SELECT (COUNT(*) AS ?n) WHERE { GRAPH <urn:cow:observations:2025> { ?s <urn:cow:value> ?v } }"
            ),
            25_000
        );
        assert_eq!(
            count(
                profile.path(),
                "SELECT (SUM(?v) AS ?n) WHERE { GRAPH <urn:cow:observations:2025> { ?s <urn:cow:value> ?v } }"
            ),
            (0..25_000u64).sum::<u64>()
        );
        // The default-graph triple went to user:rdf, not the default graph.
        assert_eq!(
            count(
                profile.path(),
                &format!("SELECT (COUNT(*) AS ?n) WHERE {{ GRAPH <{user_rdf}> {{ <urn:cow:cube> ?p ?o }} }}")
            ),
            1
        );
        assert_eq!(
            count(
                profile.path(),
                "SELECT (COUNT(*) AS ?n) WHERE { <urn:cow:cube> ?p ?o }"
            ),
            0
        );
    }

    /// Turtle goes to `--into` when given.
    #[test]
    fn triples_go_to_the_named_graph_asked_for() {
        let profile = profile_with_graph();
        let file = profile.path().join("labels.ttl");
        std::fs::write(
            &file,
            "<urn:a> <urn:label> \"a\" .\n<urn:b> <urn:label> \"b\" .\n",
        )
        .unwrap();
        let mut load = args(&file);
        load.into = Some("urn:cow:labels".to_string());
        let report = load_into_profile(profile.path(), &load, &mut |_| {}).unwrap();
        assert_eq!(report.graphs.get("urn:cow:labels"), Some(&2));
        assert_eq!(
            count(
                profile.path(),
                "SELECT (COUNT(*) AS ?n) WHERE { GRAPH <urn:cow:labels> { ?s ?p ?o } }"
            ),
            2
        );
    }

    /// A file naming a reserved projection graph refuses the whole load:
    /// the quads before it are not committed either.
    #[test]
    fn a_reserved_graph_anywhere_in_the_file_loads_nothing() {
        let profile = profile_with_graph();
        let file = profile.path().join("poison.nq");
        let mut text = String::new();
        for i in 0..1000 {
            text.push_str(&format!("<urn:s:{i}> <urn:p> \"x\" <urn:cow:fine> .\n"));
        }
        text.push_str(&format!(
            "<urn:s> <urn:p> \"x\" <{}:projection:workspace> .\n",
            crate::rdf::graph_subject(GRAPH)
        ));
        std::fs::write(&file, text).unwrap();
        let error = load_into_profile(profile.path(), &args(&file), &mut |_| {}).unwrap_err();
        assert!(error.contains("reserved projection graph"), "{error}");
        assert!(error.contains("nothing was loaded"), "{error}");
        assert_eq!(
            count(
                profile.path(),
                "SELECT (COUNT(*) AS ?n) WHERE { GRAPH ?g { ?s ?p ?o } }"
            ),
            0
        );
    }

    /// A parse error halfway through also loads nothing.
    #[test]
    fn a_broken_file_loads_nothing() {
        let profile = profile_with_graph();
        let file = profile.path().join("broken.nt");
        std::fs::write(
            &file,
            "<urn:a> <urn:p> \"a\" .\n<urn:b> <urn:p> \"unterminated .\n",
        )
        .unwrap();
        let error = load_into_profile(profile.path(), &args(&file), &mut |_| {}).unwrap_err();
        assert!(error.contains("parse"), "{error}");
        assert_eq!(
            count(
                profile.path(),
                "SELECT (COUNT(*) AS ?n) WHERE { GRAPH ?g { ?s ?p ?o } }"
            ),
            0
        );
    }

    #[test]
    fn a_missing_graph_or_unknown_format_is_refused_with_the_way_forward() {
        let profile = profile_with_graph();
        let file = profile.path().join("x.csv");
        std::fs::write(&file, "a,b\n").unwrap();
        let error = load_into_profile(profile.path(), &args(&file), &mut |_| {}).unwrap_err();
        assert!(error.contains("--format"), "{error}");
        let mut other = args(&file);
        other.graph = "nope".to_string();
        let error = load_into_profile(profile.path(), &other, &mut |_| {}).unwrap_err();
        assert!(error.contains("create it first"), "{error}");
        let mut reserved = args(&profile.path().join("x.nt"));
        reserved.into = Some(format!(
            "{}:projection:workspace",
            crate::rdf::graph_subject(GRAPH)
        ));
        std::fs::write(profile.path().join("x.nt"), "").unwrap();
        let error = load_into_profile(profile.path(), &reserved, &mut |_| {}).unwrap_err();
        assert!(error.contains("reserved"), "{error}");
    }
}
