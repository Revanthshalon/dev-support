# ops-support

Operational support for multi-service codebases. **`ops`** (CLI) indexes a workspace's code, shared
API contracts, Dockerfiles and compose files into a graph in **Memgraph**. **`ops-mcp`** (MCP server)
gives any MCP client (Claude Code, Claude Desktop, …) the tools and agent playbooks to go from
*"checkout returns 500"* to a root cause and a proposed fix. It reads logs across services, maps them
onto the code flow, checks configuration, and inspects datastores read-only.

> **Status:** scaffolding only. Implementation follows the plans in
> [`docs/superpowers/plans/`](docs/superpowers/plans/) against the design in
> [`docs/superpowers/specs/2026-10-24-ops-support-design.md`](docs/superpowers/specs/2026-10-24-ops-support-design.md).
> Commands marked *(planned)* don't exist yet.

## How it works

```
                 ops init                         ops index
 workspace ───────────────▶ ops.yaml ───────────────────────────▶ Memgraph
 (code, Dockerfiles,        .env.example     parse: Python · TS/HTML/CSS ·      (graph)
  compose, OpenAPI)         .mcp.json        Dockerfile · compose · OpenAPI        ▲
                                                                                   │ Cypher
   MCP client  ◀──── stdio ────▶  ops-mcp  ────────────────────────────────────────┘
   (the agent)                      ├─ logs: docker compose · kubectl · files
                                    ├─ datastores: Postgres · Redis (read-only)
                                    └─ LSP: pyright · typescript-language-server
```

- **No LLM runs inside ops-support.** The MCP client is the agent. ops-support supplies tools
  and **agent playbooks** (MCP prompts) that sequence them: `diagnose`, `triage_logs`,
  `explain_flow`, `check_misconfiguration`, `investigate_datastore`, `write_rca`.
- **Service boundaries.** Two services are connected when there is evidence of any of these:
  - a shared API contract (an OpenAPI document, or schema models in a shared library);
  - a shared library;
  - docker compose wiring (`depends_on`, a hostname in an env value, a shared volume);
  - an HTTP call in one service that matches a route in another.
- **Root-cause classes** the `diagnose` playbook decides between: operational procedure,
  misconfiguration, datastore, code bug, infrastructure.

### v1 scope

| Area | Supported |
|---|---|
| Languages | Python, TypeScript, JavaScript, HTML, CSS |
| Frameworks | FastAPI, Python workers (binaries / background jobs), Angular |
| Infrastructure | Dockerfile, docker compose, Kubernetes (logs, pod status and rollout history through `kubectl`, namespace allow-list) |
| Execution flow | Static error paths (raise → handler → HTTP status or job outcome), async queue hops (Redis lists, streams, pub/sub, Celery, RQ), table access (SQL literals, SQLAlchemy), cross-service request timelines from logs, recent git changes |
| Contracts | OpenAPI 3.x documents; schema libraries (pydantic `BaseModel`, TS `interface`/`type`) |
| Datastores | PostgreSQL, Redis |
| Graph store | Memgraph (through `rsmgclient`) |

## Repository layout

```
Cargo.toml              workspace: shared versions, lints
deny.toml               dependency audit policy (cargo-deny)
docker-compose.yml      Memgraph + Memgraph Lab; data bind-mounted at docker/memgraph/ (planned)
docker-compose.gpu.yml  optional GPU override for local embedding models (planned)
.env.example            credentials template (copy to .env)
services/
  engine/               library: config, discovery, parsing, graph, store, logs, datastores, LSP
  cli/                  binary `ops`
  mcp/                  binary `ops-mcp`
    templates/          incident-report.md: the template `write_rca` fills
docs/superpowers/       design spec and implementation plans
```

## Setup

### Prerequisites

| Tool | Why | Install (macOS / Debian-Ubuntu) |
|---|---|---|
| Rust (stable, ≥ 1.88) | build | `rustup` (the toolchain is pinned by `rust-toolchain.toml`) |
| cmake + C compiler | `rsmgclient` compiles the C `mgclient` library | `brew install cmake` / `apt install cmake clang` |
| OpenSSL 3 | `rsmgclient` TLS | `brew install openssl@3` / `apt install libssl-dev` |
| Docker + compose | local Memgraph | Docker Desktop / docker-ce |
| cargo-deny | dependency audit | `cargo install --locked cargo-deny` |
| kubectl *(optional)* | Kubernetes log source | `brew install kubectl` |
| Language servers *(optional)* | `lsp` tool | `npm i -g pyright typescript-language-server typescript` |

> **macOS:** `rsmgclient`'s build script looks for OpenSSL **only** under
> `/opt/homebrew/Cellar/openssl@3` (Apple Silicon) or `/usr/local/Cellar/openssl@3` (Intel), so it
> must be the Homebrew `openssl@3` formula. `OPENSSL_ROOT_DIR` is not consulted.

### First run

```sh
cp .env.example .env              # set MEMGRAPH_USER / MEMGRAPH_PASSWORD
docker compose up -d              # Memgraph on :7687, Lab UI on http://localhost:3000
cargo build --workspace
cargo deny check                  # must pass before any dependency change is merged
```

Memgraph data lives in the named volumes `memgraph_data` and `memgraph_log`. They survive
`docker compose down`; only `docker compose down -v` deletes them.
*(Planned: data moves to a bind mount at `docker/memgraph/`, see the layer design.)*

### Using it on a workspace *(planned)*

```sh
ops init --root /path/to/workspace     # writes ops.yaml, .env.example, .mcp.json; never overwrites without --force
$EDITOR /path/to/workspace/ops.yaml    # review the discovered services, libs, contracts, datastores
ops check --root /path/to/workspace    # validates config; reports missing env vars and unreachable backends
ops index --root /path/to/workspace    # parse and write the graph (add --dry-run to only print counts)
ops watch --root /path/to/workspace    # keep the graph current as files change (incremental)
ops knowledge export --root /path/to/workspace   # back up the semantic layer to ops-knowledge/ (commit it)
claude mcp add ops -- ops-mcp --root /path/to/workspace --mode operational-support
```

## Configuration — `ops.yaml`

`ops init` discovers most of this. Every credential field takes either a literal value or
`{ env: NAME }`. Env vars are read from the process environment first, then from `.env` at the
workspace root.

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
  namespaces: [shop]                                # allow-list; tools refuse other namespaces
exclude: [node_modules, .venv, dist, .angular, __pycache__, .git]
shared_libs:
  - { name: common, path: libs/common, language: python }
contracts:                                          # shared API schemas at service boundaries
  - name: orders-api
    kind: openapi                                   # openapi | library
    path: contracts/orders.openapi.yaml
    provider: orders-api
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
  level: info                                       # OPS_LOG overrides it (e.g. OPS_LOG=engine=debug)
  format: text                                      # stderr: text | json
  dir: .ops/logs                                    # rotated daily: ops-mcp.*.jsonl, ops.*.jsonl, audit.*.jsonl
  retention_days: 14
  audit: true                                       # one redacted record per MCP tool call
```

Override any value without editing the file using `OPS_`-prefixed env vars, with `__` between
nesting levels, for example `OPS_MEMGRAPH__PORT=7688`.

## Embeddings and semantic search *(planned)*

> [!NOTE]
> Designed in [`2026-10-24-persistence-and-semantic-layer-design.md`](docs/superpowers/specs/2026-10-24-persistence-and-semantic-layer-design.md) §4.5.
> Uses Memgraph's native vector indexes, text indexes and the MAGE `embeddings` module, all in the
> free Community edition. ops-support adds no ML dependency of its own.

The model name picks the path:
- **A bare name or Hugging Face path** (the default, `all-MiniLM-L6-v2`) runs locally inside
  Memgraph. No key is needed.
- **`provider/model`** (`openai/text-embedding-3-small`, `voyage/voyage-3`,
  `ollama/nomic-embed-text`, …) is called remotely through LiteLLM.

Memgraph reads provider API keys from **its own container environment**, never from `ops.yaml`.
That's why keys are set in `.env` before `docker compose up`.

### 1. When starting Docker: `.env` in this repo

| Variable | Default | Purpose |
|---|---|---|
| `EMBEDDINGS_MODEL` | `all-MiniLM-L6-v2` | Model the warmup service loads; keep it equal to `semantic.embeddings.model` |
| `HF_TOKEN` | unset | Gated or private Hugging Face models |
| `HF_HUB_OFFLINE` | `0` | `1` = never download (air-gapped; pre-seed `docker/memgraph/lib/hf-cache`) |
| `OPENAI_API_KEY` | unset | `openai/…` |
| `AZURE_API_KEY`, `AZURE_API_BASE`, `AZURE_API_VERSION` | unset | `azure/<deployment>` |
| `COHERE_API_KEY` | unset | `cohere/…` |
| `VOYAGE_API_KEY` | unset | `voyage/…` |
| `MISTRAL_API_KEY` | unset | `mistral/…` |
| `GEMINI_API_KEY` | unset | `gemini/…` |
| `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_REGION_NAME` | unset | `bedrock/…` |

Ollama needs no key; point `api_base` at it in `ops.yaml`. Any other LiteLLM provider variable
works too, because the whole `.env` is passed to the Memgraph container.

```sh
docker compose up -d                                   # defaults: local model on CPU, downloaded on first use
docker compose --profile embeddings up -d              # also download/load EMBEDDINGS_MODEL and check its key now
docker compose -f docker-compose.yml -f docker-compose.gpu.yml up -d   # NVIDIA GPU for local models (Linux)
```

### 2. Per workspace: `ops.yaml`

Every field accepts a literal or `{ env: NAME }`, and can be overridden with
`OPS_SEMANTIC__EMBEDDINGS__<FIELD>`.

```yaml
semantic:
  embeddings:
    enabled: true              # false = text search only
    model: all-MiniLM-L6-v2    # bare/HF name → local; provider/model → remote (LiteLLM)
    # local only
    device: null               # null (first GPU, else CPU) | cpu | cuda | all | "cuda:1" | 1 | [0, 1]
    batch_size: 2000
    chunk_size: 48             # per-GPU chunk, multi-GPU path
    # remote only
    api_base: null             # Ollama, Azure, proxies
    input_type: document       # how code cards and knowledge are embedded
    query_input_type: query    # how search queries are embedded (Cohere/Voyage use it)
    dimensions: null           # truncate output, if the provider supports it
    timeout: 60
    num_retries: 3
    normalize: true
    remote_batch_size: null    # null = 96 for Cohere, 256 otherwise
    concurrency: 4             # 1–32 parallel requests
    # vector index
    metric: cos
    scalar_kind: f32
    # long symbols and entries are embedded as overlapping chunks; search scores the best chunk
    chunking:
      enabled: true            # false = one chunk per symbol, cut at max_tokens
      max_tokens: auto         # auto = the model's max sequence length (all-MiniLM-L6-v2: 256)
      remote_max_tokens: 512   # used for auto with remote models, which don't report one
      overlap_tokens: 32
      max_chunks_per_symbol: 32
      strategy: syntax         # syntax (split at statements) | lines
      aggregate: max           # max | mean_top2
```

Changing `model`, `dimensions`, `metric` or `scalar_kind` rebuilds the vector indexes and
re-embeds everything on the next `ops index`. Changing the chunking settings re-chunks (re-parses)
everything. `search_code` returns the matching line range inside long functions. `ops check` reports which path is active (local, or
remote with the provider), the vector dimension, and any load or key error.

## Indexing limits *(planned)*

Files are never split for parsing; the parser needs the whole file. The size limit keeps
generated noise out of the graph, and you can change it:

```yaml
indexing:
  max_file_bytes: 1MiB          # B / KiB / MiB / GiB or bytes; 0 = no limit
  skip_minified: true           # *.min.js / *.min.css and very long-lined files
  overrides:                    # first matching glob wins
    - { path: "services/legacy/**/*.py", max_file_bytes: 8MiB }
    - { path: "**/generated/**", skip: true }
```

To set it right before a run, use `ops index --max-file-bytes 8MiB` or
`OPS_INDEXING__MAX_FILE_BYTES=8MiB ops index`. The env var also works for `ops watch`.

> [!NOTE]
> The flag lasts **one run only**. The next run without it, including `ops watch`, goes back to
> the configured limit and drops those files again. Knowledge attached to them is re-attached when
> they return. For a lasting change, use an `overrides` entry or the env var. Raising the
limit re-parses on the next run, so files that were skipped get picked up. `ops index` lists the
largest skipped files with their reason; `--show-skipped` lists all of them.

## Modes

`ops-mcp --mode <mode>` picks one bundle of tools, prompts and instructions per server process.
To use several modes, register the server several times.

| Mode | For | Status |
|---|---|---|
| `operational-support` | Incidents: logs → code flow → root cause → fix | **MVP** (default) |
| `developer-support` | Context for code changes from the service graph, combined with your LSP setup, context files, prompts and hooks | planned |
| `operational-onboarding` | Do's and don'ts for operating the system, from the graph and your runbooks; no production access | planned |
| `developer-onboarding` | A fast guided tour of the codebase | planned |

## MCP surface: `operational-support` *(planned)*

| Group | Tools |
|---|---|
| Graph and code | `service_map` (boundaries with evidence), `contract`, `find_symbol`, `code_context`, `trace_flow`, `read_file`, `graph_query` (read-only), `lsp` |
| Operations | `fetch_logs`, `match_log`, `analyze_logs`, `compose_status`, `k8s_status`, `service_config`, `check_config` |
| Data | `db_query`, `db_schema`, `redis_command`, `datastore_health` |
| Semantic layer | `remember`, `revise`, `retract`, `recall`, `knowledge_gaps`, `search_code` |
| Index and incidents | `reindex`, `save_incident_report`, `find_incident_reports` |
| Execution flow and change | `error_paths`, `request_timeline`, `recent_changes`; `service_map(focus=…)` for blast radius |

Agent prompts: `diagnose`, `triage_logs`, `explain_flow`, `check_misconfiguration`,
`investigate_datastore`, `write_rca`, `enrich_knowledge`. `write_rca` produces an incident report from
[`services/mcp/templates/incident-report.md`](services/mcp/templates/incident-report.md): symptoms
and how to recognize them, root cause with evidence, a checklist to validate a recurrence,
mitigation and fix, a checklist to validate the resolution, prevention, and a runbook entry.

## Safety model

- **Secrets never reach tool output or error text.** Everything passes through redaction, and
  config shows only whether an env var is set.
- **Postgres is read-only at the database:** `BEGIN READ ONLY` plus a statement timeout. The query
  is wrapped as a single SELECT subquery, rows are capped, and the transaction always rolls back.
  Use a read-only DB user as well.
- **Redis** accepts only an allow-list of read commands.
- **`graph_query`** rejects write clauses. The graph can always be rebuilt with `ops index`.
- **`read_file` and `lsp`** read only configured service, library and contract paths, compose files and
  Dockerfiles. `.env*`, `.ops/` and `ops.yaml` are always denied.
- **Kubernetes** calls are limited to `kubernetes.namespaces`. Arguments from the LLM are never
  passed through a shell.
- **Every tool call is audited** to `.ops/logs/audit.*.jsonl`: tool, redacted arguments (SQL and
  commands included), outcome, duration, backends touched and result size. Result content is never
  recorded.
- **`ops-mcp` starts even when every backend is down.** Tools connect on demand and report
  failures as text.

## Development

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                  # unit and fixture tests, no Docker needed
cargo test --workspace -- --ignored     # integration tests: need `docker compose up -d`
cargo deny check                        # advisories, licenses, bans, sources
```

CI (`.github/workflows/ci.yml`) runs fmt, clippy, tests and cargo-deny on every push and PR.

**Dependency policy** (`deny.toml`):
- Vulnerable, unmaintained, unsound or yanked crates fail the build.
- Only permissive licenses are allowed.
- Crates come from crates.io only.
- `neo4rs` and the sqlx MySQL/SQLite drivers are banned by design.
- Shared crate versions live in the root `[workspace.dependencies]`.
