# engine

The core library of ops-support. Everything that `ops` (CLI) and `ops-mcp` (MCP server) do lives
here: workspace configuration, discovery, parsing code and infrastructure files, building the
service graph, the Memgraph store, and the log, datastore, config-check and LSP tooling. The two
binaries are thin layers over this crate.

> [!NOTE]
> **Status: stub.** The crate declares its dependencies but has no modules yet. They are added by
> [Part 1](../../docs/superpowers/plans/2026-10-24-ops-indexer.md) (indexer) and
> [Part 2](../../docs/superpowers/plans/2026-10-24-ops-mcp.md) (MCP tooling) of the plan. The
> design is in the [spec](../../docs/superpowers/specs/2026-10-24-ops-support-design.md).

## Design rules

- **Pure core, thin I/O edges.** `graph::build` turns a workspace into nodes and edges with no
  database involved, so it's tested against fixtures alone. Only `store` talks to Memgraph.
- **Config is the source of truth.** Every module takes a loaded `Ctx` (the `ops.yaml` config,
  the workspace root and `.env` values) rather than reading the environment itself.
- **Secrets stay secret.** A credential is a literal or `{ env: NAME }`, never shows up in
  `Debug` output, and anything leaving the crate as text passes through `redact`.
- **Fail soft.** Unparseable or oversized files are counted and skipped, and an unreachable
  backend is an `Err`, never a panic.

## Modules *(planned)*

| Module | Responsibility | Plan task |
|---|---|---|
| `config` | `ops.yaml` schema, `Secret` resolution, `.env`, `OPS_*` overrides, `validate` | P1·1 |
| `redact` | Masks secret-looking values and `user:pass@` in any text | P1·1 |
| `docker` | docker compose (with anchors and merge keys) and Dockerfile parsing; service connectivity from env values | P1·2 |
| `spans` | `file:line` spans for YAML/JSON keys (tree-sitter) | P1·2 |
| `discover` | `ops init`: find services, libraries, contracts and datastores; write `ops.yaml`, `.env.example`, `.mcp.json` | P1·3 |
| `parse` | tree-sitter extraction: Python (FastAPI, workers), TypeScript/Angular, HTML templates, CSS | P1·4–5 |
| `contract` | OpenAPI 3.x endpoints and schemas | P1·5b |
| `graph` | Builds the service graph: code, docker, contracts, `BOUNDARY` summaries | P1·6 |
| `store` | Memgraph through `rsmgclient`: idempotent write, query, ping | P1·7 |
| `telemetry` | `tracing` setup: stderr plus rotated files, redacting writer | P2·8a |
| `codeq` | Graph and code queries; read-only Cypher guard; `read_file` path allow-list | P2·8 |
| `logs`, `runtime` | Log sources (compose, kubectl, file, command); compose and k8s status | P2·9 |
| `logmatch` | Log line or traceback → `LogStatement` → function; clustering | P2·10 |
| `cfgcheck` | Missing env vars, port mismatches, unreachable hosts, with `file:line` | P2·11 |
| `datastore` | Read-only PostgreSQL and allow-listed Redis | P2·12 |
| `lsp` | Minimal LSP client: definition, references, hover, document symbols | P2·13 |

## Graph model at a glance

```
(:Service)-[:DEPENDS_ON | CONNECTS_TO | SHARES_VOLUME | USES_LIB | BOUNDARY]->(:Service)
(:Service)-[:CONTAINS]->(:File)-[:DEFINES]->(:Function | :Class | :Component | :Injectable | :Schema)
(:Function)-[:CALLS]->(:Function)          (:Function)-[:LOGS]->(:LogStatement)
(:Route)-[:HANDLED_BY]->(:Function)        (:Function)-[:CALLS_API]->(:Route)
(:Service)-[:PROVIDES | CONSUMES]->(:Contract)-[:DECLARES]->(:Endpoint)-[:IMPLEMENTED_BY]->(:Route)
(:Function)-[:READS_ENV]->(:EnvVar)<-[:SETS_ENV]-(:Service)
```

The spec has the full model, including properties and node ids.

## Dependencies

| Purpose | Crates |
|---|---|
| Config and serialization | `config`, `serde`, `serde_json`, `serde_norway` |
| Parsing | `tree-sitter` with grammars `python`, `typescript`, `html`, `css`, `yaml`, `json`, `containerfile` |
| Graph store | `rsmgclient`: builds the C `mgclient` and needs cmake and OpenSSL 3 (see the [root README](../../README.md#prerequisites)) |
| Datastores | `sqlx` (postgres only), `redis` |
| Runtime and utilities | `tokio`, `anyhow`, `regex`, `shlex`, `walkdir` |

Versions are pinned in the root `Cargo.toml`, and every addition must pass `cargo deny check`.

## Development

```sh
cargo test -p engine                  # unit and fixture tests, no Docker needed
cargo test -p engine -- --ignored     # needs Memgraph (docker compose up -d) or PG_TEST_URL
cargo clippy -p engine --all-targets -- -D warnings
```

The shared test fixture, a small "shop" workspace with a FastAPI service, a worker, an Angular
frontend, a shared library, Postgres and Redis, will live in `tests/fixtures/shop/` (plan Part 1,
Task 3).
