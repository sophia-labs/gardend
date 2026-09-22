# Local Service Hosting

Garden can distribute and supervise local services without absorbing their
domain model. The local service host is the boundary: Garden owns profile-local
auth, process supervision, and loopback proxying; the service owns its API,
storage layout under its assigned data dir, and runtime behavior.

Choreograph and KG-ULTRA are first-party hosted services. They remain
standalone runtimes: the same service artifacts can run outside Garden when
given equivalent environment variables.

## Boundary

Garden owns:

- loopback bearer authentication and scoped local tokens
- local profile identity
- process start, stop, status, and bounded log tail
- private localhost port assignment
- reverse proxying under `/services/{service_id}`
- MCP endpoint and token injection for services that use Garden as their graph
  backend

Hosted services own:

- their HTTP API
- health endpoint
- runtime and sandbox behavior
- durable files below the `DATA_DIR` assigned by Garden
- service-specific schemas such as Choreograph workflow runs/telemetry and
  KG-ULTRA structural intuition records

Garden must not import service source modules or rely on repository layout. A
service is configured by a command/manifest and runtime environment.

## Service Manifest

Bundled services live under the app resources directory:

```text
resources/
  services/
    choreograph/
      service.json
      ...
    kg-ultra/
      service.json
      ...
```

The manifest shape is intentionally small:

```json
{
  "serviceId": "choreograph",
  "command": "node",
  "args": ["dist/orchestrator.js"],
  "workingDir": ".",
  "healthPath": "/health",
  "proxyMount": "/services/choreograph",
  "env": []
}
```

Relative `command` values containing a path separator and relative `workingDir`
values are resolved relative to the manifest directory. Pathless commands, such
as `node`, resolve through `PATH`.

For development, Garden can start a service without bundled resources:

```text
GARDEN_CHOREOGRAPH_COMMAND=pnpm
GARDEN_CHOREOGRAPH_ARGS="--dir /path/to/choreograph exec tsx src/orchestrator.ts"
GARDEN_CHOREOGRAPH_WORKDIR=/path/to/choreograph

GARDEN_KG_ULTRA_COMMAND=/path/to/kg-ultra/.venv/bin/python
GARDEN_KG_ULTRA_ARGS="-m kg_ultra_service.server"
GARDEN_KG_ULTRA_WORKDIR=/path/to/kg-ultra
```

or:

```text
GARDEN_CHOREOGRAPH_MANIFEST=/path/to/service.json
GARDEN_KG_ULTRA_MANIFEST=/path/to/service.json
```

## Runtime Environment

When Garden starts a service it injects:

```text
PORT=<private localhost port>
DATA_DIR=<garden profile>/services/{service_id}
MNEMOSYNE_MCP_URL=<garden loopback mcp_url>
MNEMOSYNE_MCP_ACCESS_TOKEN=<garden loopback token>
MNEMOSYNE_INTERNAL_SERVICE_SECRET=<generated per-process secret>
GARDEN_LOOPBACK_API_URL=<garden loopback api_url>
GARDEN_SERVICE_ID=<service_id>
GARDEN_PROFILE_ID=default
```

Choreograph also receives `CHOREOGRAPH_SANDBOX_MODE=local-native`.

The renderer never receives the internal service secret and does not need the
private sidecar URL.

Manifest `env` values are applied before Garden's boundary environment. A
packaged service may set metadata such as `NODE_ENV`, but it cannot override
Garden-owned values such as `PORT`, `DATA_DIR`, or
`MNEMOSYNE_INTERNAL_SERVICE_SECRET`.

## Garden Packaging

The bundled-service path is opt-in. A normal source checkout can carry only
`service.example.json`; the packaged app becomes out-of-the-box when the
release process stages concrete service artifacts and writes `service.json`.

```text
scripts/prepare-choreograph-service-resource.sh /path/to/choreograph-artifact
scripts/prepare-kg-ultra-service-resource.sh /path/to/kg-ultra-artifact
pnpm tauri build
```

That script stages:

```text
src-tauri/resources/services/choreograph/
  service.json
  choreograph/
    src/orchestrator.ts
    node_modules/
    package.json
    ...

src-tauri/resources/services/kg-ultra/
  service.json
  kg-ultra/
    .venv/
    kg_ultra_service/
    pyproject.toml
    ...
```

`tauri.conf.json` declares `resources/services/**/*` as bundled resources, so
the runtime lookup path is the same in development and in the packaged app:
`services/{service_id}/service.json`.

## Loopback API

Generic service control:

```text
GET  /api/services/{service_id}/status
POST /api/services/{service_id}/start
POST /api/services/{service_id}/stop
GET  /api/services/{service_id}/logs?tail=200
```

Canonical proxy mount:

```text
ANY /services/{service_id}/{path...}
```

Garden authenticates the caller with the loopback bearer token, drops inbound
auth headers, then forwards to the private service with:

```text
X-Internal-Service: <generated per-process secret>
X-User-ID: default
```

Choreograph convenience aliases exist for first-party UI ergonomics:

```text
GET  /api/choreograph/status
POST /api/choreograph/start
POST /api/choreograph/stop
GET  /api/choreograph/logs?tail=200
```

KG-ULTRA convenience aliases mirror the same lifecycle surface:

```text
GET  /api/kg-ultra/status
POST /api/kg-ultra/start
POST /api/kg-ultra/stop
GET  /api/kg-ultra/logs?tail=200
```

The workflow facade preserves the hosted gateway route shape expected by Studio:

```text
POST /workflows/runs              -> POST /services/choreograph/api/workflows/run
GET  /workflows/runs              -> GET  /services/choreograph/api/workflows/runs
GET  /workflows/runs/{id}         -> GET  /services/choreograph/api/workflows/runs/{id}
GET  /workflows/runs/{id}/events  -> GET  /services/choreograph/api/workflows/runs/{id}/events
```

Local telemetry uses JSON polling through authenticated fetch. Local EventSource
streaming is deliberately not part of the first boundary because EventSource
cannot send the Garden loopback bearer header.

The KG-ULTRA facade names the Garden-facing reasoning contract while still
proxying to the standalone service:

```text
POST /api/kg-ultra/intuition                  -> POST /services/kg-ultra/api/intuition
POST /api/kg-ultra/link-predictions           -> POST /services/kg-ultra/api/rank/link-predictions
POST /api/kg-ultra/graphs/{graph_id}/refresh  -> POST /services/kg-ultra/api/graphs/{graph_id}/refresh
GET  /api/kg-ultra/graphs/{graph_id}/status   -> GET  /services/kg-ultra/api/graphs/{graph_id}/status
```

KG-ULTRA should consume Garden graph snapshots through authenticated loopback
RDF/SPARQL/MCP APIs. Its private ULTRA transport representation is service-owned.
Durable model output that an agent may inspect, cite, accept, reject, or later
evaluate is represented as the served `kg-ultra-intuition` Meaningful Object
vocabulary and materialized into the reserved `:projection:kg-ultra` graph.
