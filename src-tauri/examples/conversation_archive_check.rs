//! Finite no-network reader compatibility check. No profile/DB/runtime startup.
//! The second file is independent operator/test context, not archive metadata.
use garden_lib::conversation_archive::{validate, validate_projection, ArchiveScope};
use serde::Deserialize;
use serde_json::Value;
use std::{collections::BTreeSet, io::Read};
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Context {
    owner: String,
    graph: String,
    namespace: String,
    instance: String,
    audience_evidence: Value,
    artifact_digests: BTreeSet<String>,
    readers: BTreeSet<String>,
    author_refs: BTreeSet<String>,
    parent_source: Option<Value>,
}
fn read(path: &str, cap: u64) -> Result<Vec<u8>, &'static str> {
    let f = std::fs::File::open(path).map_err(|_| "fixture_open_failed")?;
    let mut bytes = Vec::new();
    f.take(cap + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "fixture_read_failed")?;
    if bytes.len() as u64 > cap {
        return Err("fixture_capacity");
    }
    Ok(bytes)
}
fn run() -> Result<Value, &'static str> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        return Err("expected_archive_and_context_paths");
    }
    let context: Context =
        serde_json::from_slice(&read(&args[2], 1024 * 1024)?).map_err(|_| "context_invalid")?;
    let bytes = read(&args[1], 8 * 1024 * 1024)?;
    let scope = ArchiveScope {
        owner: &context.owner,
        graph: &context.graph,
        namespace: &context.namespace,
        instance: &context.instance,
        audience_evidence: &context.audience_evidence,
        artifact_digests: &context.artifact_digests,
        readers: &context.readers,
        author_refs: &context.author_refs,
        parent_source: context.parent_source.as_ref(),
    };
    let result = validate(&bytes, &scope)?;
    let triples = validate_projection(&result)?;
    Ok(
        serde_json::json!({"accepted":true,"id":result.id,"sha256":result.bytes_sha256,"messages":result.message_count,"parts":result.part_count,"emporiumShapeTriples":triples,"runtimeStarted":false,"persisted":false}),
    )
}
fn main() {
    match run() {
        Ok(v) => println!("{v}"),
        Err(code) => {
            println!(
                "{}",
                serde_json::json!({"accepted":false,"code":code,"runtimeStarted":false,"persisted":false})
            );
            std::process::exit(2);
        }
    }
}
