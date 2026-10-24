# ops-support — Design

Date: 2026-10-24 · Status: draft for review

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
- **Memgraph persistence:** named docker volumes so data survives `docker compose down`.

## Repository layout
```
Cargo.toml                 workspace: services/{engine,cli,mcp}
docker-compose.yml         memgraph + lab, named volumes
services/engine            library: config, discovery, parsing, graph, store, logs, datastores, lsp
services/cli               binary `ops`: init | index | check
services/mcp               binary `ops-mcp`: stdio MCP server (rmcp 3.7)
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
```
`ops init` also writes `.env.example` (every `env:` name referenced) and `.mcp.json`.

## Graph model
Every node carries `id` and `workspace`.
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

## MCP surface
**Tools.** Graph and code: `service_map` (boundaries with evidence), `contract`, `find_symbol`, `code_context`, `trace_flow`, `read_file`,
`graph_query`, `lsp`. Operations: `fetch_logs`, `match_log`, `analyze_logs`, `compose_status`,
`k8s_status`, `service_config`, `check_config`. Data: `db_query`, `db_schema`, `redis_command`,
`datastore_health`.
**Prompts (agents).** `diagnose`, `triage_logs`, `explain_flow`, `check_misconfiguration`,
`investigate_datastore`, `write_rca`.

## Safety rules
- Secrets never appear in tool output or error text. All output goes through `redact`.
- `db_query` is read-only at the database: `BEGIN READ ONLY` + `statement_timeout`, user SQL
  wrapped as a subquery (one SELECT only), row cap, always `ROLLBACK`.
- `redis_command` uses an allow-list of read commands.
- `graph_query` rejects write clauses (the graph can be rebuilt from source).
- Kubernetes calls are limited to namespaces in `kubernetes.namespaces`.
- LLM-supplied arguments are never passed through a shell.
- The MCP server starts with every backend down; each tool connects when called and reports
  failures as text.
