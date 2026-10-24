# engine

The core library of ops-support. Everything that `ops` (CLI) and `ops-mcp` (MCP server) do lives
here: workspace configuration, discovery, parsing code and infrastructure files, building the
service graph, the Memgraph store, and the log, datastore, config-check and LSP tooling. The two
binaries are thin layers over this crate.

> [!NOTE]
> **Status: stub.** The crate declares its dependencies but has no modules yet. They are added by
> [Part 1](../../docs/superpowers/plans/2026-10-24-ops-indexer.md) (indexer) and
> [Part 2](../../docs/superpowers/plans/2026-10-24-ops-mcp.md) (MCP tooling) of the plan. The
> design is in the [spec](../../docs/superpowers/specs/2026-10-24-ops-support-design.md) and the
> [persistence and semantic-layer design](../../docs/superpowers/specs/2026-10-24-persistence-and-semantic-layer-design.md)
> and the [execution-flow and investigation tools design](../../docs/superpowers/specs/2026-10-24-execution-flow-and-investigation-tools-design.md).

## Design rules

- **Pure core, thin I/O edges.** Graph building is pure, so it's tested against fixtures alone:
  `graph::file_graph` handles one file, `graph::workspace_graph` handles docker, config and
  contracts, and `graph::link` computes cross-file edges, plus the `Queue` and `Table` nodes they meet, from node
  properties. Only `store` (and
  the semantic and embedding modules built on it) talks to Memgraph.
- **Two layers, one graph.** The *structural* layer (`layer: "structural"`) is rebuilt from code,
  incrementally, with stable line-free ids. The *semantic* layer (`:Knowledge`) is written by MCP
  clients and is never deleted by indexing. Changed code marks its entries `stale`; removed code
  marks them `orphaned`.
- **Config is the source of truth.** Every module takes a loaded `Ctx` (the `ops.yaml` config,
  the workspace root and `.env` values) rather than reading the environment itself.
- **Secrets stay secret.** A credential is a literal or `{ env: NAME }`, never shows up in
  `Debug` output, and anything leaving the crate as text passes through `redact`.
- **Fail soft.** Unparseable or oversized files are counted and skipped, and an unreachable
  backend is an `Err`, never a panic.

## Modules *(planned)*

| Module | Responsibility | Plan task |
|---|---|---|
| `config` | `ops.yaml` schema (including `correlation` and `changes`), `Secret` resolution, `.env`, `OPS_*` overrides, `validate` | P1·1 |
| `redact` | Masks secret-looking values and `user:pass@` in any text | P1·1 |
| `docker` | docker compose (with anchors and merge keys) and Dockerfile parsing; service connectivity from env values | P1·2 |
| `spans` | `file:line` spans for YAML/JSON keys (tree-sitter) | P1·2 |
| `discover` | `ops init`: find services, libraries, contracts and datastores; write `ops.yaml`, `.env.example`, `.mcp.json` | P1·3 |
| `parse` | tree-sitter extraction: Python (FastAPI, workers incl. Celery/RQ), TypeScript/Angular, HTML templates, CSS. Also flow facts: raises, handlers and exception bases (Python and TS); queue operations and table access (Python only) | P1·4–5 |
| `contract` | OpenAPI 3.x endpoints and schemas | P1·5b |
| `graph` | Builds the service graph: code, docker, contracts, `Queue`/`Table` link nodes, `BOUNDARY` summaries | P1·6 |
| `store`, `lock` | Memgraph through `rsmgclient`: incremental index (upsert, relink, one transaction), query, ping; single-writer `.ops/index.lock` | P1·7 |
| `watch` | `ops watch`: debounced file watcher → incremental index | P1·7b |
| `chunk` | Splits long symbols and knowledge into overlapping, statement-aligned chunks within the model's token limit | P1·6 |
| `embed` | Code chunks and knowledge chunks embedded by Memgraph's MAGE `embeddings` module; vector and text indexes; text-only fallback | P1·7c |
| `semantic` | The knowledge layer: create, revise, retract, recall by anchor; drift reconciliation; `ops knowledge export/import` | P1·7d |
| `telemetry` | `tracing` setup: stderr plus rotated files, redacting writer | P2·8a |
| `codeq` | Graph and code queries; read-only Cypher guard; `read_file` path allow-list; index staleness | P2·8 |
| `search` | Hybrid search: vector + full text, rank-fused, graph-boosted | P2·8b |
| `logs`, `runtime` | Log sources (compose, kubectl, file, command); compose and k8s status | P2·9 |
| `logmatch` | Log line or traceback → `LogStatement` → function; clustering | P2·10 |
| `flow` | `error_paths`: raise → handlers up the call graph → HTTP status, job failure or retry; each hop labelled `exact`, `by_name`, `catch_all` or `unknown` | P2·10b |
| `timeline` | `request_timeline`: logs by correlation id or time window, merged by timestamp across services, with clock-skew flags | P2·10c |
| `cfgcheck` | Missing env vars, port mismatches, unreachable hosts, with `file:line` | P2·11 |
| `changes` | `recent_changes`: read-only git history mapped onto functions, routes and config files; hunk headers only, never diff text | P2·11b |
| `datastore` | Read-only PostgreSQL and allow-listed Redis | P2·12 |
| `lsp` | Minimal LSP client: definition, references, hover, document symbols | P2·13 |
| `incidents` | Save incident reports as files (never overwriting) and search past ones | P2·13b |

## Graph model at a glance

```
(:Service)-[:DEPENDS_ON | CONNECTS_TO | SHARES_VOLUME | USES_LIB | BOUNDARY]->(:Service)
(:Service)-[:CONTAINS]->(:File)-[:DEFINES]->(:Function | :Class | :Component | :Injectable | :Schema)
(:Function)-[:CALLS]->(:Function)          (:Function)-[:LOGS]->(:LogStatement)
(:Route)-[:HANDLED_BY]->(:Function)        (:Function)-[:CALLS_API]->(:Route)
(:Service)-[:PROVIDES | CONSUMES]->(:Contract)-[:DECLARES]->(:Endpoint)-[:IMPLEMENTED_BY]->(:Route)
(:Function)-[:READS_ENV]->(:EnvVar)<-[:SETS_ENV]-(:Service)
(:Function)-[:SENDS_TO | RECEIVES_FROM]->(:Queue)-[:ON]->(:Datastore)     Redis lists, streams, pub/sub; Celery; RQ
(:Function)-[:READS_TABLE | WRITES_TABLE]->(:Table)<-[:MAPS_TO]-(:Class)   SQL literals and SQLAlchemy models

(:Knowledge {kind, status: current|stale|orphaned|retracted})-[:ABOUT | STEP | FROM | TO]->(any structural node)
(:IndexRun)                                 one per index run (parse and embed fingerprints)
(:Chunk)-[:CHUNK_OF]->(symbol | :Knowledge)   embedded pieces; search scores a symbol by its best chunk
```

`CALLS` edges carry the call-site `lines`, and `BOUNDARY.via` includes `queue` and `table`. The
specs have the full model, including properties and node ids.

## Dependencies

| Purpose | Crates |
|---|---|
| Config and serialization | `config`, `serde`, `serde_json`, `serde_norway` |
| Parsing | `tree-sitter` with grammars `python`, `typescript`, `html`, `css`, `yaml`, `json`, `containerfile` |
| Graph store | `rsmgclient`: builds the C `mgclient` and needs cmake and OpenSSL 3 (see the [root README](../../README.md#prerequisites)) |
| Datastores | `sqlx` (postgres only), `redis` |
| Runtime and utilities | `tokio`, `anyhow`, `regex`, `shlex`, `walkdir` |
| Planned additions | `sha2` (file and body hashes), `notify` (`ops watch`), `uuid` (knowledge ids), `tracing` stack (logging), `chrono` (log timestamps for `request_timeline`) |

Embeddings, vector search and text search come from Memgraph itself (MAGE image, Community
edition), so the crate has no ML dependency. `recent_changes` runs the `git` CLI read-only when it's
installed; there's no git library.

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
