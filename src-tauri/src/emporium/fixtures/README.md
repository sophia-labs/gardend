# Emporium ingest fixtures (P4-WP3)

Test fixtures for the gardend emporium engine, embedded into the test binary with
`include_str!` (resolved from `CARGO_MANIFEST_DIR`, i.e. `src-tauri/`). They are
**portable** — no absolute path into a sibling repo — so `cargo test` runs
anywhere the repo is checked out.

| File | What it is | Consumed by |
|------|------------|-------------|
| `parsed.json` | The demo `ParsedWorkflow` def (camelCase). | parity test (`emporium::fixtures`) |
| `judgment.json` | The demo `JudgmentInput` (camelCase). | parity test (`emporium::fixtures`) |
| `desired_triples.golden.json` | The **canon()-normalized desired-triple SET** captured from the Python engine on `parsed.json` + `judgment.json` against an empty live graph. | parity test (`emporium::fixtures`) |
| `campaign.json` | The canonical GEPA campaign record (`gepa-manifest-judge-20260607`). | `emporium::planner::tests::campaign_*` |

## The golden desired-triple SET

`desired_triples.golden.json` is the **EXPECTED** desired-triple set, captured as a
sorted `.nt` SET (`nt`) plus the value-canonical comparison surface (`canon` =
sorted `[subject, predicate, [TAG, value]]` tuples). It is captured from the
**Python** engine (`emporium_engine.ingest.planner.plan_compute`), never from raw
INSERT strings — the bodies are parsed back into typed pyoxigraph terms first.

Key invariants of the capture:

- Driven against an **empty** live graph, so `current` is empty and the INSERT
  (`adds`) set IS the desired set (`rdfDelete == 0`).
- The `wf:scriptBlock` `SCRIPT_BLOCK` placeholder is left **UNRESOLVED**
  (`urn:wf-emit:placeholder:SCRIPT_BLOCK`) so the set is engine-agnostic — the
  applier resolves it only after `write_doc`.
- The graph-URI prefix is the platform-style `urn:mnemosyne:user:U:graph:lab`
  (the prefix the Python twin captures under). The Rust parity test runs
  `plan_compute` with the SAME prefix so the doc-URI subjects compare byte-for-byte.

The Rust parity test (`rust_desired_set_equals_python_golden_mod_canon`) drives
the Rust `plan_compute` on `parsed.json` + `judgment.json`, recovers its desired
set from the INSERT bodies, canon-normalizes, and asserts it equals this golden
SET **mod canon** (value-canonically: `xsd:long` ≡ `xsd:integer`, dateTime ≡
epoch-seconds).

## Regenerating the golden

Run the Python engine in `emporium-port` and emit all three workflow fixtures
(`parsed.json`, `judgment.json`, `desired_triples.golden.json`) in one shot:

```bash
cd /Users/vera/dev/sophia/emporium-port
uv run python - <<'PY'
import json
from pathlib import Path
import pyoxigraph as ox
from emporium_engine.vocabs import WORKFLOW_VOCABULARY as C
from emporium_engine.ingest.planner import plan_compute
from emporium_engine.ingest.terms import PLACEHOLDER_NS, canon, sha256_text

OUT = Path(
    "/Users/vera/dev/sophia/garden-emporium-phase01"
    "/src-tauri/src/emporium/fixtures"
)
PREFIX = "urn:mnemosyne:user:U:graph:lab"

PARSED = {
    "kind": "run-record", "name": "demo", "description": "d", "whenToUse": None,
    "script": "export const meta = {}\n",
    "scriptSha256": sha256_text("export const meta = {}\n"),
    "phases": [{"order": 1, "title": "Go", "detail": None}],
    "nodes": [
        {"label": "a", "phase": "Go", "phaseIndex": 1, "agentType": None, "prompt": "p1"},
        {"label": "b", "phase": "Go", "phaseIndex": 1, "agentType": None, "prompt": "p2 [output of a]"},
    ],
    "edges": [["a", "b"]], "duplicateLabels": [],
    "run": {
        "runId": "wf_test-1", "status": "completed", "startTimeMs": 1000,
        "endTimeMs": 5000, "endTimeIso": None, "totalTokens": 10, "agentCount": 2,
        "durationMs": 4000, "recordPath": "/tmp/x.json",
        "phases": [{"index": 1, "title": "Go"}],
        "agents": [
            {"label": "a", "phaseIndex": 1, "model": "m", "state": "done", "tokens": 5,
             "toolCalls": 1, "durationMs": 2000, "queuedAt": 1000, "startedAt": 1100,
             "cached": False, "agentType": None},
            {"label": "b", "phaseIndex": 1, "model": "m", "state": "done", "tokens": 5,
             "toolCalls": 1, "durationMs": 1000, "queuedAt": 3000, "startedAt": 3100,
             "cached": False, "agentType": None},
        ],
    },
}
JUDGMENT = {
    "shortId": "wf-demo", "preamble": "Demo.", "rationale": "",
    "nodeArchetypes": {"a": "NEW:alpha", "b": "NEW:alpha"},
    "newArchetypes": [{"slug": "alpha", "title": "Alpha", "role": "r", "template": "t"}],
}
LIVE = {
    "graph": "lab", "prefix": PREFIX, "folders": {}, "docs": {}, "workflows": {},
    "archetypes": {}, "contracts": {}, "runs": [], "nodesByWorkflow": {},
}

plan = plan_compute(C, PARSED, JUDGMENT, LIVE, [], [], {}, {}, None)
s = plan["summary"]
assert s["rdfDelete"] == 0 and s["rdfInsert"] > 0, s

triples = []
for st in plan["steps"]:
    if st["op"] == "sparql_update" and st["update"].startswith("INSERT DATA"):
        b = st["update"]; body = b[b.index("{") + 1 : b.rindex("}")]
        for t in ox.parse(body.encode(), format=ox.RdfFormat.N_TRIPLES):
            triples.append((t.subject.value, t.predicate.value, t.object))
assert len(triples) == s["rdfInsert"]

nt = sorted(f"<{x[0]}> <{x[1]}> {x[2]}" for x in triples)
canon_keys = sorted(
    ([cs, cp, list(cv)] for cs, cp, cv in (canon(t) for t in triples)),
    key=lambda x: json.dumps(x, ensure_ascii=False),
)

OUT.mkdir(parents=True, exist_ok=True)
(OUT / "parsed.json").write_text(json.dumps(PARSED, indent=2, ensure_ascii=False) + "\n")
(OUT / "judgment.json").write_text(json.dumps(JUDGMENT, indent=2, ensure_ascii=False) + "\n")
(OUT / "desired_triples.golden.json").write_text(json.dumps({
    "_comment": "GENERATED — do not hand-edit. Regen: see fixtures/README.md.",
    "fixture": "demo workflow (parsed.json + judgment.json, empty live)",
    "graphPrefix": PREFIX, "placeholderNs": PLACEHOLDER_NS,
    "scriptBlockPlaceholder": f"{PLACEHOLDER_NS}SCRIPT_BLOCK",
    "rdfInsert": s["rdfInsert"], "rdfDelete": s["rdfDelete"],
    "nt": nt, "canon": canon_keys,
}, indent=1, ensure_ascii=False) + "\n")
print("rdfInsert", s["rdfInsert"], "nt", len(nt), "canon", len(canon_keys))
PY
```

After regenerating, re-run the Rust parity gate:

```bash
cd /Users/vera/dev/sophia/garden-emporium-phase01/src-tauri
cargo test --lib emporium
```

## `campaign.json`

Copied verbatim from the platform repo
(`mnemosyne-platform-emporium/tests/emporium/fixtures/campaign.json`) so the
campaign convergence test reads a repo-local path. If the canonical campaign
fixture changes upstream, re-copy it here — the test resolves it from
`CARGO_MANIFEST_DIR`, never from the sibling repo.
