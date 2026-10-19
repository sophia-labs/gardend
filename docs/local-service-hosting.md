# Local Service Hosting

Garden can distribute and supervise local services without absorbing their
domain model. The local service host is the boundary: Garden owns profile-local
auth, process supervision, and loopback proxying; the service owns its API,
storage layout under its assigned data dir, and runtime behavior.

Choreograph and KG-ULTRA are first-party hosted services, but nothing in the
host is specific to them: any service with a `service.json` is supervised,
proxied at `/services/<id>` and told who calls the same way (the agent core's
`vehicle-host` is the next one). They remain standalone runtimes: the same
service artifacts can run outside Garden when given equivalent environment
variables.

## Boundary

Garden owns:

- loopback bearer authentication and scoped local tokens
- local profile identity, and telling the service who each caller is
- process start, stop, status, and bounded log tail
- supervision: starting a configured Choreograph with `gardend`, restarting a
  crashed service, and ending its whole process group
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

Bundled services live under the app resources directory. For `gardend` that is
the folder of the binary itself, with symlinks resolved: a
`/usr/local/bin/gardend` link into an unpacked release finds the release's
`services/`, not `/usr/local/bin/services/`.

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

Optional fields:

- `"autostart": false` keeps the service from starting with `gardend` (it can
  still be started with `POST /api/services/<id>/start`, and a person can turn
  it on for their profile with `gardend service enable <id>`, below). Sophia
  Compute (`choreograph`) ships with it.
- `"memberAccess": "all"` lets members (callers without `services.manage`)
  reach every path of the service, which then decides by the caller role
  gardend mints. The default, `"none"`, keeps the service to admins.
  Choreograph ignores the field (members reach its workspace chat only, see
  below).
- `"requires": {"node": "22.19"}`: the oldest Node.js the service runs on.
  Before every start gardend runs `node --version` (the manifest's command when
  that is `node`, else `node` on the PATH the service would get: the last
  `PATH` in the manifest's `env` or the profile's settings, else gardend's
  own). Without it, or with an older one, gardend does not start the service:
  its state is `unavailable`, with a `reason` for the person (no paths), it is
  not retried, and the log line names the PATH searched. Install Node.js and
  start gardend (or the service) again.
- `"displayName": "Chat"`: what the person calls the service, used in that
  reason ("Chat needs Node.js 22.19 or newer, ...").
- `"delegateCaller": true` asks for a delegation token with every request
  that names its graph (below, "Delegation tokens"): the service can then act
  for the caller on that graph through gardend's `/mcp`, instead of as itself.
  The default is false.
- `"acceptsModelKey": true` lets a caller's own model key (`X-Model-Key`)
  through to the service (below, "Public mode"). Every other service never
  sees that header. The default is false.

- `"mcp": {"path": "/mcp", "graphs": ["<graph-id>", ...], "toolPrefix": "<prefix>_", "timeoutMs": 30000}`
  lends the service's MCP tools to gardend's own `/mcp` (below, "Lent MCP
  tools"). Without it the service adds nothing to `/mcp`.

The agent core's service (`vehicle`, the self-host chat) ships in the gardend
release archives as `services/vehicle/` beside the binary, with `"requires":
{"node": "22.19"}`, `"displayName": "Chat"`, `"delegateCaller": true` (each chat
acts for its person; see "Delegation tokens") and `"acceptsModelKey": true` (a
visitor of a public Sophia talks to an agent on their own key; see "Public
mode").

A service id is lowercase letters, digits and dashes (at most 63). It names the
folder (`services/<id>/`), the mount (`/services/<id>`), the data dir and the
`GARDEN_<ID>_*` variables (`kg-ultra` → `GARDEN_KG_ULTRA_*`).

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

More than one service can be configured at once. gardend knows every id named
by a `GARDEN_<ID>_MANIFEST` or `GARDEN_<ID>_COMMAND` variable and every
`<dir>/<id>/service.json` under `GARDEN_SERVICES_DIR` (a folder of services)
and under the `services/` folder beside the binary. For one id the first match
wins: `GARDEN_<ID>_MANIFEST`, then `GARDEN_<ID>_COMMAND` (with `_ARGS` and
`_WORKDIR`), then `GARDEN_SERVICES_DIR/<id>/service.json`, then
`services/<id>/service.json` beside the binary. `gardend service install`
carries `GARDEN_SERVICES_DIR` and the `GARDEN_CHOREOGRAPH_*` variables of the
installing shell into the service it writes, like the other `GARDEN_*` knobs.

## Lent MCP tools

A service whose manifest declares `mcp` has the tools its MCP endpoint lists
(JSON-RPC over POST at `mcp.path`) appear in gardend's own `/mcp`, beside
gardend's tools, so an agent or a chat sees one surface. The cow-tools
service lends its curated `cow_*` tools this way:

```json
"mcp": { "path": "/mcp", "graphs": ["uruguay-cows"], "toolPrefix": "cow_", "timeoutMs": 30000 }
```

- **Which tools.** Only names that start with `toolPrefix` (lowercase letters,
  digits and underscores, ending in `_`) and are not one of gardend's own;
  anything else is dropped and logged. gardend asks the service for
  `tools/list` while it runs, keeps the answer for a minute, and asks again
  when the service restarts. A service that is not running lends nothing; a
  call of one of its tools then says the service is not running.
- **Who may call.** A tool the service marks read-only
  (`annotations.readOnlyHint: true`) needs `graphs.read`: any reader with
  `mcp.tools.call` may call it. Any other tool needs `services.manage` (an
  admin), or `services.proxy` (a member) when the manifest says
  `"memberAccess": "all"`. `tools/list` shows a caller only the lent tools it
  may call, each with `_meta` naming the service, its graphs and the scopes it
  needs.
- **Which graph.** The tools are about the graphs in `mcp.graphs`. A listed
  tool takes `graph_id` (an enum of those graphs; filled in when there is
  only one); the service's own `graphId`/`graph_id` arguments are taken out of
  the schema. A token bound to one graph (a delegation token) sees the tools
  only when its graph is one of them, without the argument, and a call naming
  another graph is refused. The call reaches the service without the graph
  argument and with the headers the proxy mints (`x-internal-service`,
  `x-user-id`, `x-sophia-caller-role`, `x-graph-id`, `x-graph-owner`,
  `x-sophia-graph-role`), never the caller's credential or headers.
- **How long.** gardend waits `timeoutMs` (default 30 000, at most 120 000)
  and then answers with an error (`data.code: "service_tool_timeout"`) and
  closes the request. A service should keep its own, shorter budget and cancel
  its work when it runs out, so that "timed out" means stopped.
- **Errors pass through.** A JSON-RPC error from the service reaches the
  caller with its code, message and data.
- **Public mode.** A visitor or account (and a service acting for one) is
  offered the read-only lent tools of a service that `public.json` lists in
  `services`, about the public graphs only (`graph_id`'s enum is narrowed to
  them; a call that names another graph is refused), and calls them as a
  viewer. Nothing else of the service, and nothing of a service public mode
  does not list.
- **Inert unless declared, never in a hosted cell.** With no `mcp` in any
  manifest, `/mcp` lists and dispatches exactly what it did; a hosted cell
  never asks a service for tools.

## Supervision

- **Autostart.** `gardend` (with or without `serve`) starts every configured
  service that autostarts, once its API is ready. Whether one does is the
  profile's choice when the person made one (`gardend service enable <id>` /
  `gardend service disable <id>`, kept owner-only as `on` or `off` in
  `<profile>/services/<id>.autostart`), else its manifest's `autostart`
  (default true); `GARDEN_<ID>_AUTOSTART` set to `0` (`false`, `off`, `no`)
  in gardend's environment keeps it off whatever the profile says. When a
  gardend serves the profile, `enable` starts the service at once and
  `disable` stops it, through its API. Status and `/self-host.json` say
  `enabled` for a configured service. Never in a hosted cell. With nothing
  configured nothing starts, and `gardend` behaves as before. The desktop app
  does not autostart. gardend's MCP `workflow_run_start` (admins) still
  starts Choreograph on demand.
- **Restart.** A service that crashes (a nonzero exit or a signal) while it is
  wanted is started again after a backoff: 1 s, doubling to at most 60 s. A run
  of 60 s or more starts the backoff over. After 10 crashes in a row the
  supervisor gives up (`state: "error"`) until the next start. A clean exit
  (status 0, for instance after someone sends it SIGTERM) is not a crash and is
  not restarted. Status reports `restarts` and, between a crash and its
  restart, `state: "restarting"`.
- **Unavailable.** A service whose `requires` this computer does not meet is
  not started (`state: "unavailable"`, `reason`). `/self-host.json` carries the
  reason, and the proxy answers its requests with 503
  `{"ok": false, "code": "service_unavailable", "error": "<reason>"}`.
- **Stop.** A service runs in a process group of its own. `POST .../stop`, and
  `gardend`'s own exit (SIGTERM, Ctrl-C), send SIGTERM to the whole group and
  SIGKILL to whatever is left 8 s later. `gardend` stops its services first,
  before it drains requests and runs its final flush.
- **Orphans.** gardend records the process it started in
  `<profile>/services/{service_id}.supervisor.json` (pid and the OS's start
  time). If a gardend dies without stopping it (SIGKILL, a crash), the next
  start ends that process group first. A recorded pid that now belongs to
  another process (a different start time) is left alone.

## Runtime Environment

When Garden starts a service it injects:

```text
PORT=<private localhost port>
DATA_DIR=<garden profile>/services/{service_id}
MNEMOSYNE_MCP_URL=http://127.0.0.1:<gardend port>/mcp
MNEMOSYNE_MCP_ACCESS_TOKEN=<a member-scope token minted for this process>
MNEMOSYNE_INTERNAL_SERVICE_SECRET=<generated per-process secret>
GARDEN_LOOPBACK_API_URL=http://127.0.0.1:<gardend port>
GARDEN_SERVICE_ID=<service_id>
GARDEN_PROFILE_ID=default
```

Every service also receives `GARDEN_SERVICE_HOST=127.0.0.1`: listen on
loopback only, gardend is the only client (and on WSL2 nothing outside the
distribution reaches it). Choreograph also receives
`CHOREOGRAPH_SANDBOX_MODE=local-native` and `CHOREOGRAPH_BIND_HOST=127.0.0.1`.

- The MCP and API addresses are gardend's own loopback address, never the
  public URL a gateway or a Cloudflare Tunnel puts in front of it.
- The MCP token is not the master key. gardend mints it for each process, in
  memory, with the `member` grant (every read, the authoring writes, MCP
  calls; no deletes, RDF loads or updates, or administration), and it stops
  working when the process ends. A request with it is `service:{service_id}`.
  A service that acts for people asks for delegation tokens instead (below).
- gardend's own `GARDEN_*` environment (above all `GARDEN_LOOPBACK_TOKEN`,
  the master key `gardend serve` runs with) is withheld from the service.

The renderer never receives the internal service secret and does not need the
private sidecar URL.

The environment is layered, later wins:

1. gardend's environment, without its `GARDEN_*` variables;
2. the manifest's `env`;
3. the profile's settings for every service, `<profile>/services.env`;
4. the profile's settings for this service, `<profile>/services/<id>.env`;
5. the Garden-owned values above (`GARDEN_*`, `MNEMOSYNE_*`, `PORT`,
   `DATA_DIR`, Choreograph's sandbox mode and bind host), which nothing else
   overrides.

The settings files (`KEY=VALUE` lines, `#` comments, owner-only) hold what a
shipped `service.json` cannot, above all model keys:

```text
gardend service env set ANTHROPIC_API_KEY [--service vehicle] < key.txt
gardend service env unset ANTHROPIC_API_KEY [--service vehicle]
gardend service env list [--service vehicle]      names only, never values
gardend chat key set < key.txt                    = service env set OPENROUTER_API_KEY
gardend chat key clear
```

Values come from standard input (or are pasted when asked), never from the
command line. When a gardend serves the profile, the running services the
change reaches are restarted through its API so it takes effect at once.

## Garden Packaging

**gardend release archives** carry two services beside the binary, each built
(not source) and checked against a lock in `export/`:

- `services/vehicle/` (chat): `export/vehicle-service.lock.json`,
  `scripts/build-vehicle-service.sh`, `scripts/check-vehicle-service.mjs`.
- `services/choreograph/` (Sophia Compute, workflows, opt-in):
  `export/choreograph-service.lock.json`, `scripts/build-choreograph-service.sh`
  (Choreograph's `deploy/build-gardend-service.mjs` at the locked commit:
  `app/main.mjs` the bundled orchestrator, `body/body.mjs` the agent body on
  upstream pi, `LICENSE`, `NOTICE`, `THIRD-PARTY-NOTICES.md`),
  `scripts/check-choreograph-service.mjs`. Its only native code, libSQL's
  binding, is per platform: the lock names each release target's
  (`natives`), the public tree carries none, and each archive gets its own
  (`check-choreograph-service.mjs --add-native <target>` fetches it from npm
  at the locked version and checks its sha256).

`scripts/assemble-gardend-export.sh` needs `VEHICLE_SERVICE_DIR` and
`CHOREOGRAPH_SERVICE_DIR`; the public release workflow checks both folders and
packages them into every archive.

The desktop app's bundled-service path is opt-in. A normal source checkout can carry only
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

The proxy needs `services.proxy`. Garden authenticates the caller (the master
key, a client token, a supervised service's own token, or a Cloudflare Access
login on a self-host), drops every header by which a service could learn who
calls or with what authority (`authorization`, `cookie`, `cf-access-*`,
`x-user-*`, `x-graph-*`, `x-sophia-*`, `x-internal-*`, `x-pn-*`) and forwards
with headers it mints itself. This is the whole set; a service believes it
only together with its per-process secret:

```text
X-Internal-Service: <generated per-process secret>   every request
X-User-ID: <the caller>                               every request
X-Sophia-Caller-Role: owner | editor | viewer         every request
X-Graph-ID: <graph>                                   when the request names its graph (below)
X-Graph-Owner: user:owner                             with X-Graph-ID
X-Sophia-Graph-Role: owner | editor | viewer          with X-Graph-ID
X-Sophia-Delegated-Token: sophia-dlg-...              with X-Graph-ID, if the manifest asks
```

The roles come from the caller's grant: `owner` for an admin (holds
`services.manage`: the master key, `session-all`, a listed Access admin),
`editor` for a member, `viewer` otherwise.

`X-User-ID` is `owner` for the master key, `token:<id>` for a client token
(the id `gardend token list` shows), the email of a person Access admitted,
`access-service:<client id>` for an Access service token, and
`service:{service_id}` for a supervised service's own token.

**Graph-scoped paths.** For every service but Choreograph, a request to
`/services/<id>/g/<graph_id>/<rest...>` names its graph the way a gateway route
does: gardend takes the graph off the path, forwards `/<rest...>` (or `/`), and
mints `X-Graph-ID`, `X-Graph-Owner` and `X-Sophia-Graph-Role` for it. A path
that starts with `/g` but names no graph is refused (400). The service learns
the graph from that header and never from its request body (the agent core's
`vehicle-host` binds a session to it). On a self-host every graph belongs to
the profile owner and a caller's grant reaches every graph, so this binds a
request to a graph without widening what the caller may reach.

A canonical Choreograph invocation (`POST .../api/invocations`) is bound as the
gateway binds it, whatever the page sent: `actorRef` becomes
`user:<X-User-ID>`, and a named graph becomes
`{"ownerPrincipal": "user:owner", "graphId": <its id>}`. Every graph of a
gardend belongs to the profile's owner (the identity the master key is minted
as); a member or an Access login acts on it with its own `actorRef`, as an
editor acts on a hosted owner's graph. For that request gardend also mints
`X-Graph-ID`, `X-Graph-Owner` and `X-Sophia-Graph-Role`. Choreograph refuses a
graph role without its graph, so requests that name no graph carry none of
them.

**Delegation tokens.** A service whose manifest says `"delegateCaller": true`
gets, with every graph-scoped request (and a Choreograph invocation that names
its graph), `X-Sophia-Delegated-Token`: a token for gardend's `/mcp` that acts
for that caller, on that graph, with the caller's role. A caller's own copy of
the header never passes (`x-sophia-*` is dropped).

- **What it may do:** a viewer's token holds the reader's scopes, an editor's
  or owner's a member's (reads and the authoring writes; no deletes, RDF loads
  or updates, administration), never a scope the caller lacks and never the
  service proxy. `mcp.tools.read` and `mcp.tools.call` are always there: they
  open `/mcp`, and each tool still checks its own scopes, so a viewer's token
  reads and writes nothing, whatever the service asks.
- **Where it works:** `/mcp` only, bound to its graph. `tools/list` leaves out
  the graph argument (`graphId`, `graph_id`) of every tool, the tools that
  reach beyond one graph (list or create graphs, upload host files, start
  workflows) and the tools its scopes cannot call. A call gets its graph filled
  in; a call that names another graph, anywhere in its arguments, or a job of
  another graph, is refused. These are the hosted cell's own policy classes
  (`cell_graph_boundary`), applied per token instead of per process. Anywhere
  else the token is no credential (401).
- **How long:** an hour, in memory. The same caller on the same graph keeps
  its token until half of it is gone, then gets a new one, so the newest token
  a service holds always has half an hour left. Every token dies with the
  service process it was handed to (a stop, a crash, gardend's exit).
- **Who the records name:** a write through it is the caller's. Each CRDT
  write's audit row names the caller as its `actor` with `details.via:
  service:<id>`, and each call of a write tool adds an `mcp.tools.call` row
  (actor, tool, graph, outcome). Any other MCP request on a gardend that is
  not a hosted cell names its own caller in the CRDT rows too (`owner`,
  `token:<id>`, an Access email, `service:<id>` for a service's own token);
  before, every row said `local-runtime`.

The graph is bound by the token, not by the URL: one credential carries it, so
a path and a token can never disagree, and `/mcp` keeps one shape for every
caller (as a hosted cell binds its graph through the gateway's lease, not its
path). A hosted cell never mints or accepts one: its service routes are
denied.

**Members.** The `member` grant holds `services.proxy`, so the people Access
admits as members (and `--grant member` tokens) can chat. A member reaches a
service whose manifest says `"memberAccess": "all"` in full, any other service
not at all, and Choreograph's workspace chat and nothing else: `GET /health`,
`GET /api/workflows`, the run list, a run and its events, an invocation that
starts only `workspace-chat-v1` (the envelope keys the chat panel sends, no
workflow source), `/api/agent-sessions/...` and `/api/chat-seeds`. Starting
other workflows, the workflow facade's `POST /workflows/runs`, graph
operations, KG-ULTRA and every other route answer 403 unless the caller is an
admin: a local Choreograph runs agent bodies without a sandbox
(`local-native`). Taking chat back from members is one line:
`MEMBER_SERVICE_SCOPES` in `loopback_token_grants.rs`.

Responses stream through as they arrive, server-sent events included; the
proxy buffers nothing. The service's own CORS headers are dropped (gardend
answers CORS for its origin). Path segments that would resolve elsewhere
(`.`, `..`, encoded or not) are refused.

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

Event streams (`Accept: text/event-stream`) pass through the proxy. A browser
`EventSource` cannot send the loopback bearer header, so clients read them with
an authenticated `fetch` (or, on a self-host behind Cloudflare Access, with the
Access cookie). JSON polling keeps working.

`GET /self-host.json` (self-host only, public) carries
`"services": {"<id>": {"configured": <bool>, "state": <state>}, ...}` for every
service gardend knows of (Choreograph always), so the UI knows whether chat is
there.

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

## Public mode

A self-hosted gardend is closed: the owner's key, client tokens, Cloudflare
Access. **Public mode** opens a listed set of graphs to anyone, read-only, the
way `cow-tools.sophia-labs.com` is open (hub plan
`plans/2026-10-14-cow-tools-demo.md`). It is `src-tauri/src/public_mode.rs`.

### Turning it on

Put `public.json` in the profile (beside `graphs/`), and start gardend as
usual (`gardend serve --profile <dir>`, or the installed service):

```json
{
  "graphs": ["uruguay-cows", "uruguay-catalogue", "cow-tools-guide"],
  "services": ["vehicle"],
  "siteDir": "site",
  "clientIpHeader": "x-forwarded-for",
  "rateLimit": { "perMinute": 120, "burst": 60 },
  "maxBodyBytes": 32768,
  "sparql": { "timeoutMs": 10000, "maxRows": 1000 },
  "accounts": {
    "cognito": {
      "issuer": "https://cognito-idp.us-west-1.amazonaws.com/us-west-1_BlPVDVQJx",
      "clientId": "<the app client id>",
      "domain": "auth.sophia-labs.com",
      "callbackUrl": "https://cow-tools.sophia-labs.com/auth/callback"
    }
  }
}
```

Only `graphs` is required. With no graph listed, or no file, public mode is off
and gardend is exactly what it was. Unknown fields are an error (a typo never
leaves it half-open). `GARDEN_PUBLIC_CONFIG` points at another file;
`GARDEN_PUBLIC_GRAPHS` (a comma list) replaces the file's `graphs`, and alone
turns public mode on with the defaults. The log says what it opened at start
(`public mode (...): anyone may read ...`).

It is **never on in a hosted cell** (`GARDEN_CELL_ID`: the same binary, the
cells lane): a cell ignores the file and the variables and logs that it did.
Only the headless `gardend` reads them; the desktop app never does.

| Field | Default | Meaning |
|---|---|---|
| `graphs` | (required) | The graphs anyone may read. |
| `services` | none | The local services visitors reach through `/services/<id>`, each on a public graph's path only. |
| `siteDir` | `GARDEN_SITE_DIR`, else `site/` beside the binary (beside `ui/`) | A static front page, served at `/` (below). Relative to the profile. |
| `clientIpHeader` | none (the socket's peer) | The header that names the visitor's address when gardend sits behind a proxy on the same machine (Caddy, cloudflared): `x-forwarded-for` (its last address, the one the proxy saw) or `cf-connecting-ip`. Believed only from a loopback peer. |
| `rateLimit.perMinute`, `rateLimit.burst` | 120, 60 | A token bucket per address. |
| `maxBodyBytes` | 32768 | A visitor's request body. |
| `sparql.timeoutMs`, `sparql.maxRows` | 10000, 1000 | A visitor's SPARQL ceilings (REST and the MCP tools), under the server's own. |
| `accounts.cognito` | none | Sophia accounts (below): `issuer` (the pool), `clientId` (the ID token's `aud`), `domain` and `callbackUrl` (for the page's sign-in), `jwksUrl` (default `<issuer>/.well-known/jwks.json`). |

### Who a visitor is

- **Anonymous** (no `Authorization`, no Access token, no WebSocket bearer):
  `anon:<24 hex>`, a hash of the `X-Visitor-Id` header the client sends (16 to
  128 of `[A-Za-z0-9_-]`; a browser keeps one in local storage, an agent in its
  state), else of its address and user agent. The raw id never leaves gardend.
- **A Sophia account**: `Authorization: Bearer <Cognito ID token>`, verified
  against the pool's keys (RS256, `iss`, `aud` = `clientId`, `token_use` =
  `id`, `exp` with 60 s of leeway): `cognito:<sub>`. An access token is
  refused. One global Sophia account: the same pool and app client the hosted
  app uses.

Both are **viewers of the listed graphs, and of nothing else**: they hold
`public_mode::PUBLIC_SCOPES` (reads of documents, workspace, files, images,
wires, entities, salience, search, SPARQL; the MCP door; the service proxy).
Not tokens, logs, jobs, the profile, history, memory, time travel, RDF dumps,
exports or the trash, and no write. A request with a credential of its own
(the key, a client token, a delegation, an Access login) is not a visitor's:
public mode passes it untouched, and the owner keeps every power the key gives.

### What a visitor reaches

Ahead of every handler, `public_mode_middleware`:

1. **Rate limits** per address: `429` with `Retry-After` (seconds) and
   `{"ok": false, "code": "rate_limited", "error": ...}`. Static files are
   not counted; nor are callers with a credential.
2. **Body size**: `413` (`body_too_large`).
3. **The route**, by the same policy and effect registry a hosted cell
   enforces (`cell_graph_boundary::public_route`):
   - a route every one of whose scopes is public, on a listed graph: its
     `{graph_id}`, or the graph selectors of its JSON body (`graphId`,
     `graph_id`, `sourceGraphId`, ..., all of them listed, at least one);
   - `/mcp`, the graph list (`GET /graphs`, `/api/graphs`, `.../catalog`:
     the listed graphs only, each with the role `viewer`), `/health`, and the
     listed services;
   - anything else is `403` (`public_read_only`, "This Sophia is open to
     read"), a graph not listed `404` (`graph_not_found`, as if it did not
     exist), a trash `403`.
4. Inside, each handler still checks its scopes (a write needs a scope no
   visitor holds) and SPARQL takes the lower ceilings.

**MCP.** `tools/list` shows the tools that read one graph and that the public
scopes can call, each keeping its graph argument, plus `list_graphs` (the
listed graphs only); `_meta["sophia.publicGraphs"]` names them. `tools/call`
must name a listed graph (`graphId` or `graph_id`); the call is then bound to
it as a delegation token binds one (another graph anywhere in the arguments is
refused). A write tool answers `-32003` ("not open to visitors"), a graph not
listed `-32004` ("graph not found").

**The UI.** A visitor (and an account) reads the rooms: the workspace room
and every document room of a listed graph serve their state to a caller with
no credential, and close (code `4403`) on a frame that would change anything.
An anonymous reader's presence is dropped. So the app works for visitors as it
does for the owner, read-only (and an account writes its own notes; next
section).

**Services.** A visitor reaches the services `services` lists, on a listed
graph's path (`/services/<id>/g/<graph>/...`) or on the service's own paths;
any other is `403`. gardend mints them as it mints anyone, and tells the
service every public graph:

```text
X-User-ID: anon:<hash> | cognito:<sub>
X-Sophia-Caller-Role: viewer
X-Sophia-Graph-Role: viewer                       on a graph-scoped path
X-Sophia-Delegated-Token: sophia-dlg-...          on a graph-scoped path, if the manifest asks (a viewer's)
X-Sophia-Caller-Kind: anonymous | account
X-Sophia-Account-Email, X-Sophia-Account-Email-Verified   an account's, from its ID token
X-Sophia-Public-Graphs: uruguay-cows,uruguay-catalogue,...   every public graph
X-Model-Key: <the caller's own model key>         only if the manifest says "acceptsModelKey": true
```

**One conversation over every public graph.** A delegation token made for a
visitor or an account is not bound to one graph like everyone else's: at
`/mcp` it acts as a visitor does (the public tool list, every graph argument
kept, read-only), on every public graph, and a call that names no graph reads
the graph of the path it was minted on (the conversation's own). So one chat
reads `uruguay-cows`, `uruguay-catalogue` and `cow-tools-guide` together;
vehicle tells the model which (`graphId` in each call) and fences the rest.

`X-Visitor-Id` and `X-Model-Key` are dropped for every other service, like
every credential. A delegation made for a visitor keeps the visitor's SPARQL
ceilings at `/mcp`.

**The front page.** With a `siteDir`, the site is served at `/` and the app
moves under `/app/`:

| Path | Served |
|---|---|
| `/`, `/<page>`, `/<folder>/` | the site's file, or the folder's `index.html` |
| a page with a markdown twin (`index.html` + `index.md`, `x.html` + `x.md`) | the twin when the caller ranks `text/markdown` at least as high as `text/html` or does not ask for HTML (`*/*`, an agent's bare curl), the page otherwise; `Vary: Accept` |
| `/.well-known/...` | the site's (the one dot folder a site may serve) |
| `/llms.txt`, `/robots.txt`, ... | the site's files |
| `/app` | `308` to `/app/` |
| `/app/<path>` | the app (`GARDEN_UI_DIR` or `ui/`), with its SPA fallback |
| `/assets/...` and other files the site lacks | the app's file, when it has it (an app built for `/` still finds its assets; build it for `/app/` to keep the two apart) |
| a page neither has | the site's `404.html` with `404`, when it has one |
| `/mcp`, `/graphs`, `/services/...`, every API route | the API, as always |
| `/self-host.json` | as always, with a `public` section (below) |
| `/mcp/server-card` | the site's `mcp-server-card.json` (MCP SEP-2127's place for it); `/mcp`'s `initialize` then names itself by the card (`serverInfo`: its `name`, `version`, `title`) |

Without a site, the app stays at `/`. Content types an agent reads by: `.md`
`text/markdown`, `.jsonld` `application/ld+json`, `.geojson`, `.ipynb`,
`.csv`, `.ttl`, with the usual web ones (`.json`, `.txt`, `.xml`, `.svg`, fonts).

`GET /self-host.json` carries `"public": {"graphs", "services", "site",
"app", "mcp": "/mcp", "visitorIdHeader", "modelKeyHeader", "limits": {...},
"accounts": {"kind": "cognito", "issuer", "clientId", "domain",
"callbackUrl", "tokenStorageKey"} | null, "visitorFolders": [{"graphId",
"folderId", "open", "requires"}] | null}`: what a page or an agent needs to
use this Sophia with no account, and where a signed-in one may write.

### Visitors who write: one folder for signed-in accounts

`public.json` may open **one folder of one public graph** to people signed in
with a Sophia account (verified email). There each account makes documents,
and writes, renames, moves (inside the folder), wires and trashes **its own**;
everything else stays read-only, other visitors' documents included.

```json
"visitorFolders": [ { "graphId": "cow-tools-guide", "folderId": "moo-vements-of-the-spirit",
  "quotas": { "documentsPerAccount": 20, "totalDocuments": 2000, "newDocumentsPerHour": 200,
              "writesPerMinute": 30, "documentBytes": 524288, "roomBytesPerMinute": 262144,
              "linksPerDocument": 20, "wiresPerAccount": 50 } } ],
"accounts": { "cognito": { ..., "tokenStorageKey": "cow-tools.account.tokens" } }
```

(the quotas shown are the defaults; `accounts` is required; `tokenStorageKey`
is where the site's sign-in keeps the tokens in the browser, so the app on the
same origin signs in with them).

How it holds (`src-tauri/src/visitor_fence.rs`):

- **An account holds no write scope.** A write is admitted only where the
  fence is asked: the tools `create_document` (no `documentId`: the server
  names it, it lands in the folder), `write_document`, `insert_blocks`,
  `update_blocks`, `edit_block_text`, `delete_blocks`, `edit_comment`,
  `rename` (documents), `move_documents`, `create_wires`, `delete` (blocks and
  wires); the REST trash and restore of a document; and the document rooms.
  Every other path sees a viewer.
- **The fence travels with the write**: the CRDT queue stamps it into the
  operation (a server-only key, stripped from every caller), refuses any other
  operation kind, and the executor checks ownership and place again when the
  operation runs (and on recovery).
- **Ownership** is `createdBy` on the workspace entry (`get_workspace` shows
  it as `extra.createdBy`, with `extra.createdByName`), `acct-` and a salted
  hash of the Cognito subject (never the subject, never the email), written by
  the server only.
- **Rooms.** A visitor never writes the workspace room. A document room takes
  an account's frames only for its own documents inside the folder, each one
  applied to a copy first and checked: only the `content`, `comments` and
  `metadata` parts, at most `documentBytes`, no images, query blocks or
  opaque blocks, links `http`/`https`/`mailto` only (at most
  `linksPerDocument`), comments signed as the account. Anything else closes
  the socket (`4403`) and lands nothing. Frames are capped at 256 KiB.
- **Names.** A visitor's name and colour are its collaboration-presence
  identity: the name it picks in the app's presence control, bound to the
  account by gardend (`<profile>/public/visitors.json`) and stamped on its
  presence, its documents (`createdByName`) and its comments. Latin letters
  (accents too), digits, single spaces and `. ' - _ ( ) &`, at most 40; never
  an email, an id, a name another account holds, or one that reads as the
  house's (`Owner`, `Sophia`, `admin`, `Visitor 1a2b`, ...), compared with
  accents, case, punctuation and look-alikes (`0wner`) folded; default
  "Visitor 1a2b" in the colour the app would give.
- **Quotas** count an account's documents in the Trash too (so a restore
  never overflows them).
- **The Trash is the owner's to see.** A public request (anonymous, an
  account, or a delegation made for one) reads no document in the Trash: not
  over the document routes (`404`), MCP (`-32004`), its room (`404`), or
  SPARQL (its named graph is left out of the query's dataset). The listings
  already leave it out. Moving a visitor's note to the Trash takes it out of
  public reading at once; its title stays in the workspace's own CRDT (the
  workspace room) until you delete it from the Trash.
- **The owner** keeps everything (the key opens every route), and:

| `gardend visitors ...` | |
|---|---|
| `off` / `on` | visitor writing stops / starts at the next request or room frame (the file `<profile>/public/visitor-writes.off`); reading goes on. gardend writes the file itself when more than `newDocumentsPerHour` new documents arrive in an hour |
| `status` | the folder, the switch, accounts seen, bans |
| `list` | each account: id, documents, name, first seen |
| `ban <account>` / `unban <account>` | `acct-...` (or `cognito:<sub>`): no more writes; its documents stay until you trash or delete them |

To remove a visitor's document for good, delete it from the Trash. `tools/list` tells an account where it may write:
`_meta["sophia.writeFence"]` (`graphId`, `folderId`, `account`, `name`,
`color`). A service an account reaches through the proxy hears it too
(`X-Sophia-Write-Fence: <graph>/<folder>`, `X-Sophia-Visitor-Account`), and
the account's delegation token writes inside the fence and nowhere else, as
the account (`via` the service). A hosted cell never loads public mode, so
never a fence.

### Talking to an agent: the two doors

Reading is free. A conversation costs model money, so `vehicle` (the
`vehicle-host` service, vehicle `protocol/ride-v2.md`, "Public sessions")
opens one by either of two doors:

1. **The caller's own key**: an OpenRouter key in `X-Model-Key`, sent with
   every request. The service keeps it in memory for that conversation only
   (never in a file, an event, the capsule, the mirror or pi's transcript);
   after a restart the next request brings it again. Any model of the
   provider.
2. **A Sophia account with credit**: a verified email gets the signup credit
   once ($0.50), and each run is debited by its real cost. The house key pays,
   on a short list of cheap models (DeepSeek by default). At zero: "out of
   credit: paste your OpenRouter key". A global daily cap on credit-funded
   spend ($5) guards against signup abuse.

An anonymous caller with neither is refused (`402 model_key_required`), with
both doors named. Both doors share the limits: turns per caller per day (100),
turns per conversation (40), and each run's budget (8 model turns, $0.05).
The settings are the service's (`gardend service env set ... --service
vehicle`); see vehicle's `protocol/ride-v2.md`.

### For the deployment (W4)

- **Cognito:** the demo signs people in with the one global Sophia account:
  pool `us-west-1_BlPVDVQJx`, the public app client the hosted app uses
  (`mnemosyne-dev-spa-client`: code flow with PKCE, no secret), its hosted UI
  at `auth.sophia-labs.com`. Add the demo's callback
  (`https://cow-tools.sophia-labs.com/auth/callback`) and sign-out URL to that
  client's allowed URLs, and put its client id in `accounts.cognito.clientId`.
  The page runs the PKCE exchange itself and sends the ID token as a bearer;
  gardend only verifies it.
- **Proxy:** behind Caddy on the same box, set `"clientIpHeader":
  "x-forwarded-for"` (Caddy replaces a client's own `X-Forwarded-For`), or
  `cf-connecting-ip` behind cloudflared. Without it every visitor shares the
  proxy's address and one bucket.
- **`GARDEN_PUBLIC_BASE_URL`** (`https://cow-tools.sophia-labs.com`): its
  origin is then an allowed origin, so the site's pages may call the API with
  an account's token.
