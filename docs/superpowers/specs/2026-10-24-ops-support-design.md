# ops-support — Design

Date: 2026-10-24 · Status: accepted

## Purpose
A CLI (`ops`) and an MCP server (`ops-mcp`) for operational support. `ops init` writes a
configuration describing the workspace; `ops index` turns the code, Dockerfiles and compose files
into a graph stored in Memgraph. The MCP server exposes tools and agent playbooks (MCP prompts)
that let any MCP client take a problem statement, read logs across services, compare them against
the code flow, inspect configuration and datastores, determine the root cause and propose a fix.

## Decisions (from the user)
- **No LLM inside the server.** The MCP client is the agent. "Agents" ship as MCP prompts
  (playbooks that sequence the tools).
- **v1 languages:** Python, TypeScript, JavaScript, HTML, CSS (Angular frontend).
  **v1 frameworks:** `fastapi`, `worker` (Python binary/worker services), `angular`.
  Docker: Dockerfile + docker compose parsing for service connectivity.
  Parsing: tree-sitter for code, Dockerfiles (`tree-sitter-containerfile`) and source spans of
  YAML/JSON (`tree-sitter-yaml`, `tree-sitter-json`). YAML/JSON values come from `serde_norway` /
  `serde_json`. Every finding can cite `file:line`.
- **Configurable** via the file `ops init` generates: services, shared libraries, the APIs they
  expose/consume, framework, language, log sources, datastores.
- **Credentials** come from the config file *or* the environment: every secret field is either a
  literal string or `{ env: NAME }`. A `.env` at the workspace root is read; the process env wins.
- **Log sources:** docker compose, Kubernetes (via `kubectl`, with a namespace allow-list in the
  config), file, custom command.
- **Datastores v1:** PostgreSQL, Redis.
- **Service boundaries** are first-class. Two services are connected at a boundary when there is
  evidence of any of these kinds:
  1. **Shared API schemas.** OpenAPI documents, or schema models (pydantic `BaseModel`, TS
     `interface`/`type`) in a shared library that both sides import.
  2. **Shared libraries.** Both services import the same library.
  3. **Docker compose wiring.** `depends_on`, a hostname in an env value, or a shared named volume.
  4. **Endpoint calls.** An HTTP call in one service's code matches a route in another.
- **Graph driver:** `rsmgclient` (not `neo4rs`).
- **Supply chain:** every dependency must pass `cargo deny check` (advisories, licenses, bans,
  sources), configured in `deny.toml`.
- **Persistence, incremental indexing and the semantic layer** are specified in
  [`2026-10-24-persistence-and-semantic-layer-design.md`](2026-10-24-persistence-and-semantic-layer-design.md)
  ("the layer design"), which wins where the two documents differ. In short:
  - Memgraph data is bind-mounted at `docker/memgraph/{lib,log}`.
  - `ops index` is incremental (`--full` re-parses everything), and `ops watch` keeps the index current.
  - LLM-written understanding lives in a separate, drift-tracked `:Knowledge` layer that indexing
    never deletes. It's backed up to a committed `ops-knowledge/` export.
  - Search by meaning uses Memgraph's native embeddings, vector and text indexes (Community edition).
  - Incident reports are saved as files and searched as the first reference. There's no incident
    memory in the graph.

## Repository layout
```
Cargo.toml                 workspace: services/{engine,cli,mcp}
docker-compose.yml         memgraph (bind mount docker/memgraph/) + lab; `embeddings` profile for warmup
docker-compose.gpu.yml     optional NVIDIA GPU override for local embedding models
services/engine            library: config, discovery, parsing, graph, store, logs, datastores, lsp
services/cli               binary `ops`: init | check | index [--full] | watch | knowledge export/import
services/mcp               binary `ops-mcp`: stdio MCP server (rmcp 3.7)
services/mcp/templates     incident-report.md (used by `write_rca`)
```

## Configuration — `ops.yaml` (written by `ops init`, edited by the user)
```yaml
version: 1
workspace: { name: shop, kind: monorepo }          # single | monorepo | multi-repo
memgraph:
  host: localhost
  port: 7687
  username: { env: MEMGRAPH_USER }
  password: { env: MEMGRAPH_PASSWORD }
docker:
  compose_files: [docker-compose.yml]
kubernetes:                                         # optional
  kubeconfig: ~/.kube/config                        # optional
  context: staging                                  # optional
  namespaces: [shop]                                # allow-list; tools refuse others
exclude: [node_modules, .venv, dist, .angular, __pycache__, .git]
shared_libs:
  - { name: common, path: libs/common, language: python }
contracts:                                          # shared API schemas at service boundaries
  - name: orders-api
    kind: openapi                                   # openapi | library
    path: contracts/orders.openapi.yaml             # openapi: the document; library: a shared_libs path
    provider: orders-api                            # service that serves it
    consumers: [frontend, worker]                   # optional; also inferred from imports and calls
services:
  - name: orders-api
    path: services/orders
    language: python                                # python | typescript | javascript
    framework: fastapi                              # fastapi | worker | angular | none
    uses: [common]                                  # shared libs (also inferred from imports)
    api: { prefix: /api }                           # optional path prefix the service serves under
    docker: { compose_service: orders, dockerfile: services/orders/Dockerfile }
    kubernetes: { namespace: shop, selector: app=orders, container: api }   # optional
    logs: { source: compose }                       # compose | kubernetes | file | command
    datastores: [orders-db, cache]
    lsp: [pyright-langserver, --stdio]
datastores:
  - { name: orders-db, kind: postgres, url: { env: ORDERS_DB_URL } }
  - { name: cache, kind: redis, url: "redis://localhost:6379/0", password: { env: REDIS_PASSWORD } }
logging:                                            # optional; these are the defaults
  level: info                                       # trace | debug | info | warn | error
  format: text                                      # stderr format: text | json
  dir: .ops/logs                                    # relative to the workspace root
  retention_days: 14
  audit: true                                       # per-tool-call audit records
semantic:                                           # optional; full option list in the layer design §4.5
  embeddings:
    enabled: true
    model: { env: EMBEDDINGS_MODEL, default: all-MiniLM-L6-v2 }   # "provider/model" = remote via LiteLLM
    chunking: { enabled: true, max_tokens: auto, overlap_tokens: 32 }  # long symbols embedded in overlapping chunks
incidents:
  dir: docs/incidents                               # where write_rca reports are saved
indexing:                                           # optional; layer design §3.1
  max_file_bytes: 1MiB                              # 0 = no limit; also --max-file-bytes / OPS_INDEXING__MAX_FILE_BYTES
  overrides: [{ path: "services/legacy/**", max_file_bytes: 8MiB }]
```
`ops init` also writes `.env.example` (every `env:` name referenced) and `.mcp.json`.

## Graph model
Every node carries `id` and `workspace`. Structural nodes (everything below) also carry
`layer: "structural"`.
- **Ids have no line numbers**, e.g. `sym:{service}:{path}:{qualname}`, so they're stable across edits.
- Symbols carry `body_hash` and `card` (the code-card header). Their embeddings live on
  `(:Chunk)-[:CHUNK_OF]->(symbol)` nodes (header plus a slice of the body), so long functions
  stay searchable end to end.
- Files carry their content `hash`.
- `(:IndexRun)` records each run.

The semantic layer (`:Knowledge` with `ABOUT`/`STEP`/`FROM`/`TO`/`SUPERSEDES`) is defined in the
layer design §4.
```
(:Service {name, kind: service|library|infra, language, framework, image})
(:Service)-[:DEPENDS_ON]->(:Service)              compose depends_on
(:Service)-[:CONNECTS_TO {via}]->(:Service)       env value names another service's host
(:Service)-[:SHARES_VOLUME {volume}]->(:Service)  same named compose volume
(:Service)-[:USES_LIB]->(:Service {kind:library}) config `uses` + imports naming the lib
(:Service)-[:USES_DATASTORE]->(:Datastore {kind})
(:Service)-[:CONTAINS]->(:File {path, language})-[:DEFINES]->(:Function {name, qualname, start_line, end_line, signature, entrypoint})
(:File)-[:DEFINES]->(:Class|:Component|:Injectable {name})-[:HAS_METHOD]->(:Function)
(:Function)-[:CALLS]->(:Function)                 name-resolved
(:Function)-[:LOGS]->(:LogStatement {level, template, line})
(:Route {method, path})-[:HANDLED_BY]->(:Function)
(:Function)-[:CALLS_API {method, path}]->(:Route) HttpClient/httpx/requests call matched to a route
(:Component)-[:INJECTS]->(:Injectable)
(:Component)-[:TEMPLATE_BINDS {event}]->(:Function)   (click)="save()" in the template
(:Function)-[:READS_ENV]->(:EnvVar {name})
(:Service)-[:SETS_ENV {source}]->(:EnvVar)        compose environment / env_file / Dockerfile ENV
(:Contract {name, kind: openapi|library, path})
(:Service)-[:PROVIDES]->(:Contract)<-[:CONSUMES]-(:Service)
(:Contract)-[:DECLARES]->(:Endpoint {method, path})    from OpenAPI `paths`
(:Endpoint)-[:IMPLEMENTED_BY]->(:Route)               same method + normalized path in the provider
(:Contract)-[:DEFINES_SCHEMA]->(:Schema {name})       OpenAPI components / BaseModel / TS interface
(:Function)-[:USES_SCHEMA]->(:Schema)                 parameter or return type annotation
(:Service)-[:BOUNDARY {via: [contract|shared_lib|compose|http], evidence}]->(:Service)
                                                  derived summary: one edge per service pair
```
`BOUNDARY` is computed after all other edges exist. `evidence` lists the underlying edge ids, so
`service_map` can show both the summary and the proof.

## Server modes
`ops-mcp` runs in exactly one **mode** per process: `ops-mcp --mode <mode>`, default
`operational-support`. A mode is a fixed bundle of three things:
- the tools it exposes;
- the prompts (agents) it exposes;
- the server `instructions` sent at `initialize`.

Keeping each mode's tool list small and on-purpose keeps the client LLM focused. To use several
modes, register several servers (e.g. `ops` and `ops-dev` in `.mcp.json`). All modes share one
graph, one config, one logging/audit pipeline and one safety model.

| Mode | For | What it adds | Status |
|---|---|---|---|
| `operational-support` | On-call / SRE: an incident or a "this is broken" report | Everything in **MCP surface** below: logs → code flow, config checks, read-only datastores, k8s/compose status, RCA agents | **MVP** |
| `developer-support` | A developer making a change | Context for code changes from the graph's full view of service orchestration, combined with the user's own setup (see below) | later |
| `operational-onboarding` | A new on-call engineer | General do's and don'ts for operating this system, from the graph plus the team's runbooks. No production access tools. | later |
| `developer-onboarding` | A new developer | A fast tour of the codebase: topology, entrypoints, request flows, where things live | later |

**developer-support (later).**
- **Tools:** the graph and code tools, plus:
  - `impact_analysis(target)`: blast radius of a symbol, file, route or schema across services, through `CALLS`, `CALLS_API`, contract consumers and `USES_SCHEMA`.
  - `change_context(files)`: the services, contracts, env vars, datastores and routes a change touches.
  - `conventions(service)`: how this service already does routes, logging, config and errors, with examples from the graph.
- **The user's own setup.** These are declared in `ops.yaml` under `developer:` and re-exposed rather than replaced:
  - **LSP:** the `lsp` commands already in each service's config.
  - **Context files** (`CLAUDE.md`, `AGENTS.md`, `.github/copilot-instructions.md`, …): served as MCP resources and cited by the prompts.
  - **The user's prompt files** (a directory of `.md`): each one becomes an MCP prompt, prefixed `user:`.
  - **Hooks:** the user's hook/check commands (lint, test, format) are listed per service, so the agent knows what must pass. The MCP server can't run client-side hooks; it only reports them.
- **No** datastore or Kubernetes tools in this mode.

**operational-onboarding (later).**
- **Prompts:** `ops_dos_and_donts`, `explain_topology`, `incident_walkthrough` (a dry run of a past incident).
- **Facts from the graph:** shared datastores and volumes, services everything depends on (fan-in hot spots), and env vars every service needs.
- **The team's runbooks:** configured under `onboarding.runbooks: [docs/runbooks/*.md]`.
- **Tools:** read-only views only: `service_map`, `contract`, `service_config`, and `check_config` against the repo. No `db_query`, `redis_command`, `fetch_logs` or `k8s_status`.

**developer-onboarding (later).**
- **Prompts:** `codebase_tour`, ordered from entrypoints and routes outward; `explain_area(path|service)`; `where_is(question)`; `first_change_guide`.
- **Tools:** the graph and code tools plus `lsp`. No operational tools.

The plans implement `operational-support` only. The `--mode` flag and per-mode registration ship in
the MVP, so later modes add bundles without changing the server's shape. Each later mode gets
its own plan when it is started.

## MCP surface
This section is the `operational-support` mode, the MVP.

**Tools.** Graph and code: `service_map` (boundaries with evidence), `contract`, `find_symbol`, `code_context`, `trace_flow`, `read_file`,
`graph_query`, `lsp`. Operations: `fetch_logs`, `match_log`, `analyze_logs`, `compose_status`,
`k8s_status`, `service_config`, `check_config`, `reindex`. Data: `db_query`, `db_schema`, `redis_command`,
`datastore_health`. Semantic layer: `remember`, `revise`, `retract`, `recall`, `knowledge_gaps`,
`search_code`. Incidents: `save_incident_report`, `find_incident_reports`.
**Prompts (agents).** `diagnose`, `triage_logs`, `explain_flow`, `check_misconfiguration`,
`investigate_datastore`, `write_rca`, `enrich_knowledge`. `diagnose` starts from past incident reports
and recalled knowledge, and ends by `remember`ing durable facts.

**Incident report.** `write_rca` fills `services/mcp/templates/incident-report.md` (header, summary,
impact, timeline, symptoms and how to recognize them, root cause with causal chain and evidence,
how to validate a recurrence, mitigation and permanent fix, how to validate the resolution,
prevention, a 5-minute runbook entry, open questions). The validation sections are checklists of
read-only tool calls with pass/fail conditions, so the next on-call can run them as written.

## Logging and audit
ops-support is itself a production tool. It runs SQL, reads logs and calls kubectl, so its own
logging is part of the design.

**Three streams, one pipeline.** Everything goes through `tracing`, and every record passes
through `redact` before it reaches any sink.

| Stream | Sink | Content | Level control |
|---|---|---|---|
| Diagnostics | stderr (`text`/`json`), plus a daily-rotated `<dir>/ops-mcp.YYYY-MM-DD.jsonl` | Startup, config load, backend connects/failures, subprocess exit codes, timings | `logging.level`; the `OPS_LOG` env var overrides it (EnvFilter syntax, e.g. `OPS_LOG=engine=debug`) |
| MCP client log | MCP `notifications/message` (the server declares the `logging` capability) | `warn`+ by default: backend unreachable, caps hit (rows/lines truncated), namespace rejected | The client's `logging/setLevel`. Nothing is sent before the client sets a level or `initialized` completes. |
| Audit | `<dir>/audit.YYYY-MM-DD.jsonl`, always on when `audit: true`, independent of `level` | One record per tool call (below) | none |

**Audit record** (one JSON line per `tools/call`):
`ts, session_id, client {name, version}, tool, args, outcome: ok|error, error?, duration_ms, backends: ["memgraph", "postgres:orders-db", "kubectl:shop", …], result {bytes, rows?, lines?, truncated}`.
- `args` are redacted and each string is truncated to 2 KiB. SQL text, Cypher, Redis commands and
  `grep` patterns *are* recorded (redacted), because what was asked of a production datastore is
  the point of an audit.
- Result *content* (rows, log lines, file text) is never recorded, only its size.
- `session_id` is a random id per server process; `client` comes from `initialize`.

**Rules**
- `ops-mcp` never writes anything to stdout except MCP protocol messages.
- Logging can't take the server down. If the log dir isn't writable, the server logs one warning
  to stderr and carries on without file sinks.
- `ops init` adds `.ops/` to the workspace `.gitignore`.
- The `ops` CLI uses the same pipeline (stderr plus the file sink `ops.YYYY-MM-DD.jsonl`), with
  `-v`/`-q` stepping the level. It writes no audit records; it runs no agent tools.
- Retention: on startup, rotated files older than `retention_days` are deleted.

## Safety rules
- Secrets never appear in tool output or error text. All output goes through `redact`.
- `db_query` is read-only at the database: `BEGIN READ ONLY` + `statement_timeout`, user SQL
  wrapped as a subquery (one SELECT only), row cap, always `ROLLBACK`.
- `redis_command` uses an allow-list of read commands.
- `graph_query` rejects write clauses. Writes from MCP go only through `remember`/`revise`/`retract`
  (schema-checked, redacted, audited) and `reindex`.
- Embedding-provider API keys live only in the Memgraph container's environment (`.env` at
  `docker compose up`), never in `ops.yaml` or tool arguments.
- `read_file` / `lsp` paths must fall under configured service, library and contract paths or be
  compose files or Dockerfiles. `.env*`, `.ops/` and `ops.yaml` are always denied.
- Kubernetes calls are limited to namespaces in `kubernetes.namespaces`.
- LLM-supplied arguments are never passed through a shell.
- The MCP server starts with every backend down; each tool connects when called and reports
  failures as text.
