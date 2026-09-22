# KG-ULTRA Garden Integration

KG-ULTRA is a structural-reasoning service for Garden graphs. Its job is not to
decide truth or mutate the graph. Its job is to produce defeasible structural
intuition: ranked links, missing-edge candidates, related-node hints, and graph
shape anomalies that an agent or user can inspect.

## Boundary

Garden owns:

- graph authority and projection graphs
- loopback/MCP auth and service-token injection
- graph snapshots, RDF/SPARQL export, and accepted writes
- the `kg-ultra-intuition` Meaningful Object vocabulary
- validation/materialization of durable intuition records

KG-ULTRA owns:

- RDF-to-ULTRA lowering
- literal/skolemization policy for model input
- relation-graph construction
- model/checkpoint loading
- per-graph caches under its assigned `DATA_DIR`
- ranking/scoring APIs

Garden must not import KG-ULTRA source modules or depend on its repository
layout. KG-ULTRA must not write accepted Garden wires directly. Accepting a
candidate goes through Garden's ordinary Wire/workspace authority path.

## Data Layers

Three layers stay separate:

```text
Garden RDF / Meaningful Objects
  -> service snapshot/export
  -> KG-ULTRA private integer graph + relation graph
  -> ranked structural intuition
  -> kg-ultra-intuition Meaningful Object
```

The private integer graph, tensors, relation graph, and checkpoint state are not
Meaningful Objects. They are implementation details of the service.

The durable output is a Meaningful Object when it can be inspected, cited,
accepted/rejected, or evaluated. That is the `kgultra:Intuition` record and its
`kgultra:IntuitionCandidate` children in `:projection:kg-ultra`.

## Local Service API

Garden starts and proxies KG-ULTRA through the generic local service host:

```text
GET  /api/kg-ultra/status
POST /api/kg-ultra/start
POST /api/kg-ultra/stop
GET  /api/kg-ultra/logs?tail=200
ANY  /services/kg-ultra/{path...}
```

Facade routes:

```text
POST /api/kg-ultra/intuition
POST /api/kg-ultra/link-predictions
POST /api/kg-ultra/graphs/{graph_id}/refresh
GET  /api/kg-ultra/graphs/{graph_id}/status
```

The service receives the same boundary environment as other local services:
`PORT`, `DATA_DIR`, `MNEMOSYNE_MCP_URL`, `MNEMOSYNE_MCP_ACCESS_TOKEN`,
`MNEMOSYNE_INTERNAL_SERVICE_SECRET`, `GARDEN_LOOPBACK_API_URL`,
`GARDEN_SERVICE_ID`, and `GARDEN_PROFILE_ID`.

## Intuition MO

The served vocab is `kg-ultra-intuition` with namespace:

```text
http://mnemosyne.dev/kg-ultra#
```

It has two classes:

- `kgultra:Intuition`: one model-attributed event over a graph snapshot.
- `kgultra:IntuitionCandidate`: one ranked head/relation/tail suggestion.

The projection sink is:

```text
urn:mnemosyne:local:graph:{graph_id}:projection:kg-ultra
```

This means a KG-ULTRA result can be materialized through the existing Emporium
generic projection lane:

```json
{
  "vocab": "kg-ultra-intuition",
  "dry_run": false,
  "payload": {
    "kind": "generic",
    "records": [
      {
        "kind": "Intuition",
        "localId": "turn-abc",
        "graphId": "garden-graph",
        "intuitionId": "turn-abc",
        "modelId": "ultra_4g",
        "modelVersion": "zero-shot",
        "sourceSnapshotId": "workspace@42",
        "taskId": "agent-turn-abc",
        "queryJson": "{\"relationWhitelist\":[\"requires\"]}",
        "generatedAt": 1782326400000,
        "wasAttributedTo": "urn:sophia:observer:kg-ultra"
      },
      {
        "kind": "IntuitionCandidate",
        "localId": "c1",
        "intuitionId": "turn-abc",
        "candidateId": "c1",
        "partOfIntuition": "urn:mnemosyne:local:graph:garden-graph:projection:kg-ultra:intuition:turn-abc",
        "head": "urn:mnemosyne:local:document:a",
        "relation": "http://mnemosyne.ai/vocab#requires",
        "tail": "urn:mnemosyne:local:document:b",
        "rank": 1,
        "score": 0.82,
        "explanation": "structural cluster match"
      }
    ]
  }
}
```

The JSON field names are predicate local names. `prov:wasAttributedTo` is
represented as `wasAttributedTo` by the generic planner's local-name mapping.

## First Useful Product Shape

The initial service should be a hybrid structural signal, not a standalone wire
suggester:

- popularity prior gives the cheap floor
- semantic search gives text/content affinity
- KG-ULTRA gives graph-structural affinity

Agents can ask for intuition during a turn using active documents/entities and a
relation whitelist. Garden/Choreograph can pass transient turn facts as request
JSON, but should not persist chain-of-thought. If the returned intuition is worth
keeping, persist only the `kg-ultra-intuition` MO summary.
