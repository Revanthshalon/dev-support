# ops-support Part 1: Config, Init and Indexer — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `ops init` writes `ops.yaml`, `.env.example` and `.mcp.json` for a workspace. `ops index` parses Python, Angular (TS/HTML/CSS), Dockerfiles, compose files and OpenAPI into a persisted Memgraph graph, **incrementally** (only changed files are re-parsed). The graph includes the flow facts Part 2's investigation tools need: exceptions (Python and TS), plus queue hops and table access (Python), as `Queue` and `Table` nodes. It embeds code cards for semantic search, and keeps the LLM-written semantic layer anchored and drift-tracked. `ops watch` keeps the index current. `ops knowledge export/import` backs up the semantic layer. `ops check` reports missing credentials, unreachable backends and the embedding status.

**Architecture:** All logic lives in the `engine` library. `cli` is a thin clap binary. Graph building is pure:
- `graph::file_graph` handles one file;
- `graph::workspace_graph` handles docker, config and contracts;
- `graph::link` computes cross-file edges from node properties alone.

So it's tested without a database. `store` is the only module that talks to Memgraph (rsmgclient, synchronous, wrapped in `spawn_blocking`). It runs the incremental upsert, relink and reconcile cycle. The structural layer (`layer: "structural"`) is rebuilt from code; the semantic layer (`:Knowledge`) is never deleted by indexing.

**Tech Stack:** Rust 2024, clap 4.6 (derive), config 0.15 (yaml), serde, serde_norway 0.9, serde_json, regex, walkdir, shlex, tree-sitter 0.27 + tree-sitter-python 0.25, tree-sitter-typescript 0.23, tree-sitter-html 0.23, tree-sitter-css 0.25, tree-sitter-yaml 0.7, tree-sitter-json 0.24, tree-sitter-containerfile 0.9, rsmgclient 4.0, tokio, anyhow. Added by tasks: `globset` (Task 1), `sha2` (Task 4), `notify` (Task 7b), `uuid` (Task 7d).

**Spec:** `docs/superpowers/specs/2026-10-24-ops-support-design.md`, plus `docs/superpowers/specs/2026-10-24-persistence-and-semantic-layer-design.md` ("the layer design"; it wins where the two differ), and `docs/superpowers/specs/2026-10-24-execution-flow-and-investigation-tools-design.md` ("the flow design")

## Global Constraints
- The workspace stays `services/{engine,cli,mcp}`. The `cli` binary is named `ops`, and the `mcp` binary is named `ops-mcp`.
- Shared crate versions go in root `[workspace.dependencies]`; member crates use `{ workspace = true }`.
- No `neo4rs`. Grammars are exactly python, typescript, html, css, yaml, json and containerfile (all already in the workspace). No MySQL or SQLite drivers.
- A secret field is `Secret = Literal(String) | { env: String }`. Secret values never appear in `Debug` output, logs or errors.
- Node ids are deterministic, unique within a workspace, and **contain no line numbers** (see Task 6), so semantic anchors survive edits.
- Every structural delete matches `layer: "structural"`. No indexing path deletes or rewrites a `:Knowledge` node. Only the reconcile step (Task 7d) changes its `status` and `ABOUT` edges.
- Indexing runs under the single-writer lock `.ops/index.lock` (Task 7).
- Every file skipped on purpose (excluded dir, over `indexing.max_file_bytes` (default 1 MiB), minified, an override `skip`, or unparseable) is reported with its reason, never fatal. Files are never chunked for parsing; only embeddings are chunked (layer design §3.1, §4.5.1).
- Mark each deliberate shortcut with a `// ponytail:` comment naming its limit and upgrade path.
- Tests that need Docker/Memgraph are `#[ignore]` and run with `cargo test -- --ignored`.
- Every task that adds or changes a dependency ends with `cargo deny check` passing. If a license isn't on the allow-list, stop and ask; don't widen `deny.toml` yourself.
- Each task ends with `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --check` clean.

## Review Focus
1. **Re-running `ops init`** on a workspace whose `ops.yaml` the user edited must not overwrite it without `--force`. Test: `init_refuses_overwrite` (Task 3).
2. **`node_modules`, `.venv`, minified bundles, huge generated files** must not be indexed and must not blow up memory. Test: `build_skips_excluded_and_large` (Task 6).
3. **Duplicate node ids** from two same-named functions in one file (nested `def`, overloads, `@property` getter and setter) would silently merge nodes. Test: `ids_are_unique` (Task 6).
4. **Missing env var for a secret**: `ops check` and `Secret::resolve` must name the variable and never print a value. Test: `secret_missing_env_names_var` (Task 1).
5. **Editing one file and re-indexing** must give exactly the graph a `--full` run would, touch only that file, and leave semantic entries on unchanged code untouched. Tests: `incremental_equals_full` (Task 7), `knowledge_survives_reindex` (Task 7d), both ignored and needing Memgraph.

---

### Task 0: Project setup (verify) and the Memgraph compose stack

The workspace setup below is already in the repo; only check it. The compose changes in Step 3 are new.

- Root `Cargo.toml`: members `services/{engine,cli,mcp}`; `[workspace.package]` (edition 2024, rust-version, `publish = false`); every shared crate version in `[workspace.dependencies]`; workspace lints (`unsafe_code = "forbid"`, clippy `all` warn).
- Binaries: `cli` → `ops`, `mcp` → `ops-mcp`. `engine` is a library stub that already declares every Part 1 dependency, including the tree-sitter grammars for python, typescript, html, css, yaml, json and containerfile.
- `deny.toml`: advisories (vulnerable/unmaintained/yanked = deny), license allow-list, bans (wildcards deny, multiple-versions warn), sources (crates.io only).
- `docker-compose.yml` (before this task): memgraph on named volumes; credentials from `.env`.
- `.env.example`, `.gitignore`, `rust-toolchain.toml`, and a `README.md` covering overview, architecture, configuration and setup.
- CI: `.github/workflows/ci.yml` runs fmt, clippy, test and `cargo deny check`.

- [ ] **Step 1: Verify.** `cargo build --workspace && cargo deny check && docker compose config -q`. Expected: all exit 0.
- [ ] **Step 2: Branch** for Part 1: `git checkout -b feat/indexer`.
- [ ] **Step 3: Compose stack** (layer design §1 and §4.5(a)).
  - **memgraph service:**
    - Bind mounts `./docker/memgraph/lib:/var/lib/memgraph` and `./docker/memgraph/log:/var/log/memgraph`. Remove the named volumes and the top-level `volumes:`.
    - `env_file: [{path: .env, required: false}]`, and `HF_HOME=/var/lib/memgraph/hf-cache`.
    - `command`: `--data-recovery-on-startup=true --storage-snapshot-interval-sec=300 --storage-snapshot-on-exit=true` (flag names verified in Memgraph v3.14.0 `src/flags/general.hpp`), plus the existing log flags.
    - Image pinned to `memgraph/memgraph-mage:3.14.0`.
  - **`embeddings-warmup` service** (`profiles: [embeddings]`):
    - Waits for Bolt, then runs `CALL embeddings.model_info({model_name: $EMBEDDINGS_MODEL}) YIELD info RETURN info;` through `mgconsole`, using the image's own if it has one, else `memgraph/mgconsole`.
    - Exits non-zero on failure.
  - **`docker-compose.gpu.yml`:** sets the memgraph image to `memgraph/memgraph-mage:3.14.0-relwithdebinfo-cuda`, since the default image's PyTorch is CPU-only, and adds `deploy.resources.reservations.devices: [{driver: nvidia, count: all, capabilities: [gpu]}]`.
  - **`.env.example`:** add every variable in layer-design table (a), commented, with its default.
  - **`.gitignore`:** update the `/docker/` comment to "Memgraph bind mount: data, logs, model cache".
  - **README:** add the Linux `chown` note. Find the uid with `docker run --rm <image> id memgraph`.
- [ ] **Step 4: Verify.**
  - Without Docker running: `docker compose config -q`, `docker compose --profile embeddings config -q`, and `docker compose -f docker-compose.yml -f docker-compose.gpu.yml config -q`. All exit 0.
  - With Docker:
    1. `docker compose --profile embeddings up -d`. `docker compose logs embeddings-warmup` shows the model info, and the service exits 0.
    2. `docker compose down && docker compose up -d`. Data created before the restart (`CREATE (:Probe {x:1})`) is still there.
    3. `docker/memgraph/lib/hf-cache` contains the model.
- [ ] **Step 5: CI job for the Memgraph-backed tests.** Add an `integration` job to `.github/workflows/ci.yml`:
  - A `services: memgraph: image: memgraph/memgraph-mage:3.14.0` container with ports `7687:7687` and `MEMGRAPH_USER`/`MEMGRAPH_PASSWORD` env.
  - It waits for Bolt, then runs `OPS_SEMANTIC__EMBEDDINGS__ENABLED=false cargo test --workspace -- --ignored --skip embed_ --skip pg_ --skip pyright_` (the core promises: incremental, reconcile, crash and lock tests). The env var makes every run text-only, so nothing downloads a model.
  - A second step, gated by `if: github.event_name == 'schedule' || contains(github.event.pull_request.labels.*.name, 'embeddings')`, runs `cargo test --workspace -- --ignored embed_`. It caches `~/.cache/huggingface` and sets `HF_HOME` in the service.

  **Naming rule for ignored tests:** a test that needs an embedding model starts with `embed_`, one that needs Postgres with `pg_`, one that needs pyright with `pyright_`. `pg_` and `pyright_` tests stay manual.
- [ ] **Step 6: Commit.** `git commit -am "chore: memgraph bind mount, recovery, embeddings warmup, gpu override, integration CI"`

---

### Task 1: Config schema, secrets, `.env`, redaction

**Files:**
- Create: `services/engine/src/config.rs`, `services/engine/src/redact.rs`

**Interfaces:**
- Produces:
  - `config::Config { version: u32, workspace: Workspace, memgraph: MemgraphCfg, docker: DockerCfg, kubernetes: Option<K8sCfg>, exclude: Vec<String>, shared_libs: Vec<Lib>, contracts: Vec<Contract>, services: Vec<Service>, datastores: Vec<Datastore> }`. Field names and nesting follow the spec's YAML exactly.
  - `Workspace { name, kind }`
  - `MemgraphCfg { host: String, port: u16, username: Option<Secret>, password: Option<Secret> }`
  - `DockerCfg { compose_files: Vec<String> }`
  - `K8sCfg { kubeconfig: Option<String>, context: Option<String>, namespaces: Vec<String> }`
  - `Lib { name, path, language }`
  - `Contract { name, kind: ContractKind /* openapi|library */, path, provider: Option<String>, consumers: Vec<String> }`
  - `#[serde(untagged)] enum EnvOr<T> { Literal(T), Env { env: String, default: Option<T> } }` and `ctx.setting<T: FromStr + Clone>(&EnvOr<T>) -> Result<T>`: env (process, then `.env`), else `default`, else an error naming the variable.
  - `SemanticCfg { embeddings: EmbeddingsCfg }`. Every `EmbeddingsCfg` field is an `EnvOr<_>` with the layer design's §4.5(b) defaults: `enabled` (true), `model` ("all-MiniLM-L6-v2"), `device` (null), `batch_size` (2000), `chunk_size` (48), `api_base` (null), `input_type` ("document"), `query_input_type` ("query"), `dimensions` (null), `timeout` (60), `num_retries` (3), `normalize` (true), `remote_batch_size` (null), `concurrency` (4; validate 1–32), `metric` ("cos"), `scalar_kind` ("f32"). `device` is an untagged enum: null, a string, an int, or a list.
  - `IncidentsCfg { dir: String /* "docs/incidents" */ }`
  - `ByteSize(u64)`, parsed from `"1MiB"`, `"512KiB"`, `"8MiB"`, `"2GiB"`, `"1048576"` or `0`, where 0 means no limit. Bad units are an error naming the field.
  - `IndexingCfg { max_file_bytes: EnvOr<ByteSize> /* 1MiB */, skip_minified: bool /* true */, overrides: Vec<Override { path: String /* glob */, max_file_bytes: Option<ByteSize>, skip: bool }> }`, plus `indexing::limit_for(cfg, relpath) -> Limit { max_bytes: Option<u64>, skip: bool }`: first matching glob wins. Add `globset` and run `cargo deny check`.
  - `ChunkingCfg { enabled: bool /* true */, max_tokens: MaxTokens /* Auto | N */, remote_max_tokens: usize /* 512 */, overlap_tokens: usize /* 32 */, max_chunks_per_symbol: usize /* 32 */, strategy: Strategy /* syntax | lines */, aggregate: Aggregate /* max | mean_top2 */ }` as `EmbeddingsCfg.chunking` (`#[serde(default)]`). Validate `overlap_tokens < max_tokens` when `max_tokens` is a number.
  - `Config` gains `indexing: IndexingCfg` (`#[serde(default)]`).
  - `CorrelationCfg { patterns: Vec<String> /* the 3 defaults in flow design §2.1 */, clock_skew_ms: u64 /* 500 */ }` and `ChangesCfg { since: String /* "7d" */ }`, as `Config.correlation` and `Config.changes` (`#[serde(default)]`). `validate` compiles each pattern and reports one without a named `id` group, and checks `since` against `^\d+[dhw]$` (max 90d).
  - `Config` gains `semantic: SemanticCfg` and `incidents: IncidentsCfg`, both `#[serde(default)]`.
  - **Partial sections.** Every section struct (`SemanticCfg`, `EmbeddingsCfg`, `ChunkingCfg`, `IncidentsCfg`, `IndexingCfg`, `Override`, `CorrelationCfg`, `ChangesCfg`) carries container-level `#[serde(default)]`, with a `Default` impl holding the spec defaults (the three correlation patterns included). The spec's own YAML has sections with only some keys (`correlation` with just `clock_skew_ms`, `chunking` with 3 of 7 keys, an override without `skip`), and they must parse.
  - `Service { name, path, language, framework: Framework, uses: Vec<String>, api: Option<Api{prefix: String}>, docker: Option<ServiceDocker{compose_service: Option<String>, dockerfile: Option<String>}>, kubernetes: Option<ServiceK8s{namespace, selector, container: Option<String>}>, logs: Option<LogsCfg>, datastores: Vec<String>, lsp: Option<Vec<String>> }`
  - `enum Framework { Fastapi, Worker, Angular, None }` (serde lowercase)
  - `LogsCfg { source: LogSourceKind /* compose|kubernetes|file|command */, file: Option<String>, command: Option<Vec<String>> }`
  - `Datastore { name, kind: DatastoreKind /* postgres|redis */, url: Secret, password: Option<Secret> }`
  - `#[serde(untagged)] enum Secret { Literal(String), Env { env: String } }`, with `impl Secret { fn resolve(&self, ctx: &Ctx) -> anyhow::Result<String>; fn env_name(&self) -> Option<&str> }` and a manual `Debug` that prints `Secret(env=NAME)` or `Secret(***)`.
  - `Ctx { root: PathBuf, config: Config }` (+ private dotenv map), with:
    - `Ctx::load(start: &Path) -> Result<Ctx>`: walks up to `ops.yaml`, overlays `OPS_` env through the `config` crate, reads `.env`.
    - `ctx.env(name) -> Option<String>`: process env first, then `.env`.
    - `ctx.service(name) -> Result<&Service>`: matches `name` or `docker.compose_service`.
    - `ctx.datastore(name) -> Result<&Datastore>`
    - `ctx.lib(name) -> Option<&Lib>`
    - `ctx.abs(rel: &str) -> PathBuf`
  - `Datastore::connection_url(&self, ctx) -> Result<String>`: puts `password` into the URL's userinfo when set.
  - `redact::redact(text: &str) -> String`
  - `config::validate(&Config) -> Vec<String>`: human-readable problems.

- [ ] **Step 1: Write the failing tests** (in `config.rs` `mod tests` and `redact.rs` `mod tests`)
```rust
#[test] fn parses_spec_example() { let c: Config = serde_norway::from_str(SPEC_YAML).unwrap();
  assert_eq!(c.services[0].framework, Framework::Fastapi);
  assert!(matches!(c.datastores[1].url, Secret::Literal(_)));
  assert_eq!(c.kubernetes.unwrap().namespaces, vec!["shop"]); }
#[test] fn secret_missing_env_names_var() { let err = Secret::Env{env:"NOPE_X".into()}.resolve(&ctx_tmp()).unwrap_err().to_string();
  assert!(err.contains("NOPE_X")); }
#[test] fn secret_debug_hides_literal() { assert!(!format!("{:?}", Secret::Literal("hunter2".into())).contains("hunter2")); }
#[test] fn semantic_defaults_and_env() { /* ops.yaml without `semantic:` → model "all-MiniLM-L6-v2", concurrency 4, metric "cos";
  with `model: {env: EMBEDDINGS_MODEL, default: "all-MiniLM-L6-v2"}` and EMBEDDINGS_MODEL=openai/text-embedding-3-small in .env → resolves to the openai model;
  concurrency 40 → validate reports it; incidents.dir defaults to "docs/incidents" */ }
#[test] fn byte_size_and_overrides() { assert_eq!("8MiB".parse::<ByteSize>().unwrap().0, 8 << 20); assert_eq!("0".parse::<ByteSize>().unwrap().0, 0); assert!("8MB?".parse::<ByteSize>().is_err());
  /* deserializes from YAML string "8MiB", YAML integer 8388608, and env string "8388608" alike */
  /* overrides [legacy/**/*.py → 8MiB, **/generated/** → skip]: limit_for("services/legacy/a/x.py").max_bytes == Some(8<<20); limit_for("x/generated/y.ts").skip; default → Some(1<<20); max_file_bytes 0 → None */ }
#[test] fn correlation_and_changes_defaults() { /* defaults: 3 patterns, skew 500, since "7d"; `correlation: {clock_skew_ms: 900}` alone keeps the 3 default patterns; a pattern without (?P<id>…) → validate problem; since "1y" → problem; the traceparent default extracts "4bf92f3577b34da6a3ce929d0e0e4736" from "traceparent: 00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01" */ }
#[test] fn chunking_defaults() { /* no chunking section → enabled, MaxTokens::Auto, overlap 32, max_chunks 32, Strategy::Syntax, Aggregate::Max; overlap 300 with max_tokens 256 → validate problem */ }
#[test] fn ops_env_override() { /* tmp ops.yaml port 7687; with OPS_MEMGRAPH__PORT=7688 set for the test → ctx.config.memgraph.port == 7688 (u16; may need config's try_parsing(true)) */ }
#[test] fn dotenv_fallback_and_process_env_wins() { /* tmp dir with ops.yaml + .env FOO=a; assert ctx.env("FOO")==Some("a") */ }
#[test] fn connection_url_injects_password() { /* url "redis://localhost:6379/0", password Literal("p@ss") → "redis://:p%40ss@localhost:6379/0" */ }
#[test] fn validate_flags_unknown_refs() { /* service uses "nope", datastores ["missing"], framework angular with language python, contract provider "ghost", library contract whose path is no shared_libs path → 5 problems */ }
#[test] fn redacts() { let s = redact("MEMGRAPH_PASSWORD=test postgres://u:hunter2@db/x \"api_key\": \"abc\" user=bob");
  assert!(!s.contains("hunter2") && !s.contains("abc") && !s.contains("=test")); assert!(s.contains("user=bob")); }
```
`SPEC_YAML` is the YAML block from the spec, pasted as a `const`.
- [ ] **Step 2: Run.** `cargo test -p engine config redact`. Expected: FAIL, because the types don't exist yet.
- [ ] **Step 3: Implement the types and functions above.**
  - The `.env` parser handles `KEY=VALUE`, `export KEY=VALUE`, `#` comments and quoted values. It never calls `std::env::set_var`.
  - The `redact` regex masks values whose key contains `password|passwd|secret|token|api[_-]?key|private[_-]?key|dsn|credential`, and masks `scheme://user:PASS@`.
  - Percent-encode the injected password by hand (`@ : / % ?` and space). Don't add a crate for it.
- [ ] **Step 4: Run.** Same command. Expected: PASS.
- [ ] **Step 5: Commit.** `git commit -am "feat(engine): config schema, secrets, redaction"`

---

### Task 2: Compose and Dockerfile parsing

**Files:**
- Create: `services/engine/src/docker.rs`, `services/engine/src/spans.rs`

**Parsing split.** Values come from `serde_norway`, which handles YAML anchors and `<<:` merge keys. tree-sitter-yaml and tree-sitter-json supply only source lines, so findings can cite `file:line`. Dockerfiles are parsed entirely with tree-sitter-containerfile.

**Interfaces:**
- Produces:
  - `spans::Spans` (`BTreeMap<String, usize>`): dotted key path → 1-based line, with sequence items indexed (`services.orders.environment.0`, `services.orders.environment.DATABASE_URL`).
  - `spans::yaml(src: &str) -> Spans`, `spans::json(src: &str) -> Spans`. They tolerate parse errors and return what they can.
  - `docker::ComposeService { name, image: Option<String>, build: Option<String> /* workspace-relative context */, dockerfile: Option<String>, depends_on: Vec<String>, environment: Vec<(String,String)>, env_files: Vec<String>, ports: Vec<String>, volumes: Vec<String> /* named volumes only */, file: String, line: usize /* the service key */, env_lines: BTreeMap<String, usize> /* env key → line, inline env only */ }`
  - `docker::parse_compose(src: &str, base: &Path, file: &str) -> Result<Vec<ComposeService>>`
  - `docker::load_compose(ctx: &Ctx) -> Result<Vec<ComposeService>>`: every `docker.compose_files` entry, with `env_file` contents merged into `environment` (file values lose to inline ones).
  - `docker::connections(svc, all) -> Vec<(String /*target*/, String /*env key*/)>`
  - `docker::Dockerfile { stages: Vec<Stage { name: Option<String>, base_image: String, line: usize }>, expose: Vec<(u16, usize)>, env: Vec<(String, String, usize)>, entrypoint: Vec<String> /* ENTRYPOINT ++ CMD, exec or shell form */, entrypoint_line: Option<usize> }` (the final stage; `line` is 1-based)
  - `docker::parse_dockerfile(src: &str) -> Dockerfile`
  - `docker::normalize(p: &Path) -> String` (lexical `..`/`.` cleanup)

- [ ] **Step 1: Failing tests**
```rust
#[test] fn compose_short_and_long_syntax() { // depends_on as list AND as map; environment as list AND map; build as string AND {context, dockerfile}
  assert_eq!(svcs["api"].depends_on, ["db"]); assert_eq!(svcs["api"].build.as_deref(), Some("services/api")); }
#[test] fn compose_named_vs_bind_volumes() { // "pgdata:/var/lib/postgresql/data" kept, "./x:/y" dropped
  assert_eq!(svcs["db"].volumes, ["pgdata"]); }
#[test] fn connections_from_env() { // api env DATABASE_URL=postgres://u:p@db:5432/x, REDIS=redis://cache:6379; also NOT matching "dbname=foo"
  assert_eq!(connections(&api, &all), [("cache".into(),"REDIS".into()), ("db".into(),"DATABASE_URL".into())]); }
#[test] fn dockerfile_cmd_forms() { assert_eq!(parse_dockerfile("FROM python:3.12\nEXPOSE 8000\nENV A=1 B=2\nENTRYPOINT [\"uvicorn\"]\nCMD [\"app.main:app\",\"--port\",\"8000\"]").entrypoint, ["uvicorn","app.main:app","--port","8000"]);
  assert_eq!(parse_dockerfile("CMD python -m worker.run").entrypoint, ["python","-m","worker.run"]); }
#[test] fn spans_point_at_lines() { let s = spans::yaml("services:\n  api:\n    environment:\n      - A=1\n      B: 2\n"); assert_eq!(s["services.api"], 2); assert_eq!(s["services.api.environment.0"], 4);
  assert_eq!(spans::json("{\n \"paths\": {\n  \"/a\": {}\n }\n}")["paths./a"], 3); }
#[test] fn compose_env_lines_and_anchors() { /* x-common: &env {LOG: debug}; api: environment: {<<: *env, DB_URL: postgres://db}
  → api.environment has LOG and DB_URL (merge key honored); api.env_lines["DB_URL"] == its line; api.line == line of "api:" */ }
#[test] fn dockerfile_heredoc_and_stages() { let d = parse_dockerfile("FROM node:20 AS build\nRUN <<EOF\nnpm ci\nEOF\nFROM python:3.12\nENV A=1\nEXPOSE 8000\nCMD [\"python\",\"-m\",\"w\"]\n");
  assert_eq!(d.stages.len(), 2); assert_eq!(d.stages[0].name.as_deref(), Some("build")); assert_eq!(d.env, [("A".into(), "1".into(), 6)]);
  assert_eq!(d.expose, [(8000, 7)]); assert_eq!(d.entrypoint_line, Some(8)); }
#[test] fn this_repos_compose_parses() { /* parse ../../docker-compose.yml (after Task 0's bind mounts): memgraph has volumes [] (bind mounts aren't named volumes), lab depends_on ["memgraph"] */ }
```
- [ ] **Step 2: Run.** `cargo test -p engine docker`. Expected: FAIL.
- [ ] **Step 3: Implement.**
  - Compose goes through `serde_norway::Value`; the schema is too loose for derive.
  - Sort `connections` output so tests are deterministic. The hostname regex is `(^|[/@=\s,])NAME(:\d+|/|$|\s|,)`.
  - Dockerfile: walk the tree-sitter-containerfile tree (`from_instruction`, `env_instruction`, `expose_instruction`, `cmd_instruction`, `entrypoint_instruction`; check the exact node kinds in the grammar's `node-types.json`). Support `ENV k=v k2=v2` and the legacy `ENV k v` form. Split shell-form CMD with `shlex`. A later CMD/ENTRYPOINT replaces an earlier one, `ENTRYPOINT ++ CMD` is concatenated, and the last stage wins.
  - Compose: values from `serde_norway::Value`, after applying merge keys. Fill `line`/`env_lines` from `spans::yaml` using `services.<name>` and `services.<name>.environment.<KEY|index>`.
- [ ] **Step 4: Run.** Expected: PASS.
- [ ] **Step 5: Commit.** `git commit -am "feat(engine): compose and Dockerfile parsing"`

---

### Task 3: `ops init` discovery and file generation

**Files:**
- Create: `services/engine/src/discover.rs`, `services/cli/src/main.rs` (init subcommand), `services/engine/tests/fixtures/shop/**` (shared fixture, used by Tasks 3–7 and Part 2)

**Fixture `tests/fixtures/shop/`** (keep each file minimal):
- `docker-compose.yml`:
  - `frontend` (build ./frontend)
  - `orders` (build ./services/orders; env `DATABASE_URL=postgres://app:secret@db:5432/orders`, `REDIS_URL=redis://cache:6379/0`; depends_on db, cache; volume `uploads:/data`)
  - `worker` (build ./services/worker; volume `uploads:/data`; env `REDIS_URL=redis://cache:6379/0`)
  - `db` (image postgres:16)
  - `cache` (image redis:7)
- `services/orders/pyproject.toml` with fastapi. `services/orders/Dockerfile` with `CMD ["uvicorn","app.main:app"]`.
- `services/orders/app/main.py`:
  - `router = APIRouter(prefix="/orders")`
  - `@router.post("/{order_id}/pay") def pay(order_id, db=Depends(get_db))` calls `charge(order_id)`, and inside `except` calls `logger.error("payment failed for order %s", order_id)`.
  - `def charge(...)` reads `os.getenv("PAYMENT_GATEWAY_URL")`.
  - `app.include_router(router, prefix="/api")`.
- **Flow fixture additions** (flow design §7), all in the files above:
  - `services/orders/app/main.py`:
    - `class PaymentGatewayError(Exception)`;
    - `charge` raises it, and reads `select(Order)`;
    - `pay` wraps `charge(...)` in `try/except Exception:`, logs, then `raise HTTPException(status_code=502)`, and runs `db.execute("INSERT INTO payments (id) VALUES (%s)", order_id)` and `r.lpush("jobs:pay", order_id)`;
    - `@app.exception_handler(ValueError) async def on_value_error(req, exc)` returns `JSONResponse(status_code=422, …)`;
    - `refund_order` calls `refund.delay(order_id)`.
  - `libs/common/common/models.py` adds `class Order(Base): __tablename__ = "orders"`.
  - `services/worker/worker.py`: `run()` loops on `r.brpop("jobs:pay")`, and `@celery.task(name="orders.refund") def refund(oid)` has `autoretry_for=(TimeoutError,)`.
- `services/orders/app/refunds.py`: one long function `reconcile_refunds`, about 200 lines of simple statements (loops over refunds, ledger updates). Its **last** block contains a comment and a log line mentioning "refund ledger". It's used by the chunking tests in Task 6, Task 7c and Part 2 Task 8b.
- `services/worker/requirements.txt`, and `services/worker/worker.py` with `def run(): log.warning(f"retrying job {job_id}")` and `if __name__ == "__main__": run()`.
- `libs/common/pyproject.toml` and `libs/common/common/db.py` with `def get_db()`. `orders/app/main.py` does `from common.db import get_db`.
- `libs/common/common/models.py`: `class PayRequest(BaseModel): amount: int`. `pay` takes `body: PayRequest` (`main.py` imports `from common.models import PayRequest`), and `worker.py` imports `from common.models import PayRequest`.
- `services/orders/openapi.yaml`: OpenAPI 3.1 with `paths: /api/orders/{order_id}/pay: post` and `components.schemas.PayRequest`.
- `frontend/package.json` with `@angular/core`. `frontend/src/app/orders.service.ts`: an `@Injectable()` class whose `pay(id)` calls `this.http.post(\`${environment.api}/api/orders/${id}/pay\`, {})` and logs `console.error('pay request failed', e)`.
- `frontend/src/app/orders.component.ts`: `@Component({selector:'app-orders', templateUrl:'./orders.component.html'})` with constructor injection `private orders: OrdersService` and a method `onPay()`.
- `frontend/src/app/orders.component.html`: `<button (click)="onPay()">Pay</button>`. `frontend/src/app/orders.component.css`: `.pay { color: red }`.
- `frontend/node_modules/x/index.js` (must be skipped).

**Interfaces:**
- Consumes: `config::*`, `docker::load_compose`/`parse_compose`, `docker::parse_dockerfile`
- Produces:
  - `discover::discover(root: &Path) -> Result<Config>`
  - `discover::write_files(root: &Path, cfg: &Config, force: bool) -> Result<Vec<PathBuf>>`: writes `ops.yaml` (with a header comment block explaining each field), `.env.example` and `.mcp.json`. Returns the paths it wrote.
  - `discover::env_names(cfg: &Config) -> Vec<String>` (sorted, deduped). Includes `EnvOr` env names (e.g. `EMBEDDINGS_MODEL`) as well as secrets.
  - `ops init` writes `semantic.embeddings.model: {env: EMBEDDINGS_MODEL, default: all-MiniLM-L6-v2}`, `incidents.dir: docs/incidents`, `correlation.clock_skew_ms: 500` with the three default `patterns` as a comment, and `changes.since: 7d` into the generated `ops.yaml`. It never git-ignores `ops-knowledge/`.
  - `discover::write_agent_instructions(root: &Path) -> Result<Vec<PathBuf>>`: for `ops init --agent-instructions` only (layer design §4.6). Appends a block between `<!-- ops-support -->` and `<!-- /ops-support -->` markers to each of `AGENTS.md` and `CLAUDE.md` that exists, or creates `AGENTS.md` when neither does. The block tells the agent to `recall` before changing or diagnosing a service, and to `remember` what it verified. A file that already has the marker is left as is.

- [ ] **Step 1: Failing tests** (`services/engine/tests/discover.rs`; copy the fixture to a tempdir with `std::fs` recursion, no new crate)
```rust
#[test] fn discovers_shop() { let c = discover(&fixture()).unwrap();
  let names: Vec<_> = c.services.iter().map(|s| (s.name.as_str(), s.framework)).collect();
  assert_eq!(names, [("frontend", Framework::Angular), ("orders", Framework::Fastapi), ("worker", Framework::Worker)]);
  assert_eq!(c.shared_libs[0].name, "common");
  assert_eq!(c.services[1].docker.as_ref().unwrap().compose_service.as_deref(), Some("orders"));
  assert_eq!(c.datastores.iter().map(|d| (d.name.as_str(), d.kind)).collect::<Vec<_>>(), [("cache", DatastoreKind::Redis), ("db", DatastoreKind::Postgres)]);
  assert_eq!(c.services[1].datastores, ["cache", "db"]);
  assert_eq!(c.workspace.kind, "monorepo");
  assert_eq!((c.contracts[0].kind, c.contracts[0].provider.as_deref()), (ContractKind::Openapi, Some("orders"))); }
#[test] fn init_refuses_overwrite() { let d = tmp_fixture(); write_files(&d, &cfg, false).unwrap();
  std::fs::write(d.join("ops.yaml"), "edited").unwrap();
  assert!(write_files(&d, &cfg, false).is_err()); assert_eq!(std::fs::read_to_string(d.join("ops.yaml")).unwrap(), "edited");
  write_files(&d, &cfg, true).unwrap(); }
#[test] fn written_config_round_trips() { /* Ctx::load(tmp) after write_files succeeds; env_names contains "MEMGRAPH_USER","MEMGRAPH_PASSWORD","DB_DATABASE_URL","CACHE_URL"; config.changes.since == "7d", config.correlation.clock_skew_ms == 500; ops.yaml text contains "traceparent" in a comment */ }
#[test] fn agent_instructions_opt_in() { /* write_files alone → AGENTS.md / CLAUDE.md untouched (absent stays absent); tmp CLAUDE.md "# mine" + write_agent_instructions twice → it starts with "# mine", contains "recall", and has the marker exactly once */ }
```
- [ ] **Step 2: Run.** `cargo test -p engine --test discover`. Expected: FAIL.
- [ ] **Step 3: Implement the discovery rules.**
  - **Units:** dirs containing `pyproject.toml`, `requirements.txt`, `setup.py` or `package.json`, skipping `exclude` dirs.
  - **Language:** Python for the Python manifests. For `package.json`: `@angular/core` gives `typescript` + `angular`, otherwise `typescript` when `tsconfig.json` exists, else `javascript`.
  - **Python framework:** `fastapi` if the manifest mentions fastapi. Otherwise `worker` if any `.py` has `if __name__ == "__main__"` or the Dockerfile entrypoint starts with `python`. Otherwise it's a shared lib when its path contains `lib|libs|shared|common|packages`, else `none`.
  - **Compose link:** a compose service whose `build` equals the unit path. The unit name is the compose service name when linked, else the dir name.
  - **Datastores:** compose services whose image starts with `postgres` or `redis`.
    - Name = compose name; `url = { env: "<NAME>_DATABASE_URL" }` for postgres, `{ env: "<NAME>_URL" }` for redis (NAME uppercased, `-` → `_`).
    - A service lists a datastore when `docker::connections` reaches it.
  - **Contracts:** each `openapi.{yaml,yml,json}` or `*.openapi.{yaml,yml,json}` (with a top-level `openapi:` key) becomes `kind: openapi`, with `provider` = the service whose path contains it, else unset. Library contracts are not written by init; Task 6 derives them.
  - **Workspace kind:** one unit → `single`; one `.git` at root or none → `monorepo`; several `.git` dirs → `multi-repo`.
  - **LSP defaults:** python `[pyright-langserver, --stdio]`; ts/js `[typescript-language-server, --stdio]`.
  - **Logs default:** `compose` when linked, else none. When `kubectl config current-context` succeeds, write a commented-out `kubernetes:` example in the header comment, not live config.
  - **`.mcp.json`:** `{"mcpServers":{"ops":{"type":"stdio","command":"ops-mcp","args":["--mode","operational-support"]}}}`. It has no `--root`, because neither client documents the server's working directory; `ops-mcp` finds the workspace by walking up to `ops.yaml` (see Part 2 Task 8). Claude Code and Copilot CLI both read this file.
  - **`.gitignore`:** append `.env` if a `.gitignore` exists and lacks it.
  - Sort services and datastores by name.
- [ ] **Step 4: Add the CLI subcommand.** `ops init [--root DIR] [--force] [--agent-instructions]` prints each written path, then: "edit ops.yaml, copy .env.example to .env, run `ops check`".
- [ ] **Step 5: Run.** The tests, plus `cargo run -p cli -- init --root <tmp copy of fixture>`. Expected: PASS, and three files listed.
- [ ] **Step 6: Commit.** `git commit -am "feat: ops init discovery"`

---

### Task 4: Python extractor (FastAPI and worker)

**Files:**
- Create: `services/engine/src/parse/mod.rs`, `services/engine/src/parse/python.rs`, `services/engine/src/parse/common.rs`

**Interfaces:**
- Produces (in `parse/mod.rs`; Task 5 adds to the same types):
  - `enum Lang { Python, TypeScript, JavaScript, Html, Css }` and `Lang::from_path(&Path) -> Option<Lang>` (`.py`, `.ts`, `.js`/`.mjs`/`.cjs`, `.html`, `.css`; `.d.ts`, `*.spec.ts` and `*.min.js` give `None`)
  - `FileFacts { imports: Vec<String>, symbols: Vec<Symbol>, env_reads: Vec<String> /* module level */, template: Option<TemplateFacts>, css_selectors: Vec<String> }`
  - `Symbol { kind: SymbolKind, name: String, owner: Option<String>, start_line: usize, end_line: usize, signature: String, decorators: Vec<String>, calls: Vec<Call { name: String, line: usize }>, logs: Vec<LogStmt>, routes: Vec<Route>, http_calls: Vec<HttpCall>, env_reads: Vec<String>, injects: Vec<String>, entrypoint: bool, selector: Option<String>, template_url: Option<String>, qualname: String /* Owner.name or outer.inner */, doc: Option<String> /* docstring / JSDoc, ≤ 1 KiB */, body_hash: String /* sha256 hex of the symbol's non-comment tokens joined by one space */, raises: Vec<Raise { type_name: String, line: usize, status: Option<u16> }>, handlers: Vec<Handler { types: Vec<String> /* [] = catch-all */, try_start: usize, try_end: usize, line: usize, action: HandlerAction /* Reraise | Return | Log | Swallow */, status: Option<u16>, reraise_as: Option<String> }>, bases: Vec<String>, handles_exception: Option<(String, Option<u16>)>, queue_ops: Vec<QueueOp { op: QueueDir /* Send | Receive */, queue: Option<String> /* None for a Celery send */, task_ref: Option<String> /* Celery send: receiver fn name, or a send_task literal */, flavor: Flavor /* RedisList | RedisStream | Pubsub | Celery | Rq */, line: usize }>, db_ops: Vec<DbOp { table: Option<String> /* SQL */, model: Option<String> /* ORM: model class name, resolved in link */, access: Access /* Read | Write */, via: Via /* Sql | Orm */, line: usize }>, table: Option<String> /* __tablename__ */, task: Option<TaskInfo { name: String, autoretry_for: Vec<String>, bound_retry: bool }> }`. `Symbol::call_names() -> impl Iterator<Item = &str>` is a helper.
  - `enum SymbolKind { Function, Method, Class, Component, Injectable, Schema }`. `Schema` = pydantic `BaseModel` subclass, TS `interface`, or exported TS `type` alias.
  - `Symbol.uses_types: Vec<String>`: the type names in parameter and return annotations (generics unwrapped: `list[PayRequest]` → `PayRequest`; builtins dropped).
  - `LogStmt { level: String /* trace|debug|info|warn|error */, template: String, line: usize }`
  - `Route { method: String, path: String /* full, router prefix applied */, handler: Option<String> }`
  - `HttpCall { method: String, path: String /* placeholders normalized to {} */, line: usize }`
  - `TemplateFacts { bindings: Vec<(String /*event*/, String /*method*/)>, elements: Vec<String> /* custom tags app-* */ }`
  - `parse::parse_file(lang: Lang, src: &str) -> FileFacts`
  - `parse::common::template_regex(template: &str) -> Option<String>`: `None` when the literal text is under 6 chars.
  - `parse::common::normalize_path(p: &str) -> String`: `${…}`, `{x}`, `:x` and `<x>` become `{}`; scheme/host and `${environment.*}` prefixes are dropped; trailing `/` is stripped.

- [ ] **Step 1: Failing tests** (`parse/python.rs` `mod tests`, using the fixture files through `include_str!`)
```rust
#[test] fn fastapi_route_with_router_and_include_prefix() { let f = parse_file(Lang::Python, MAIN_PY);
  let pay = sym(&f, "pay"); assert_eq!(pay.routes[0].method, "POST"); assert_eq!(pay.routes[0].path, "/api/orders/{order_id}/pay");
  assert!(pay.call_names().any(|c| c == "charge") && pay.call_names().any(|c| c == "get_db")); // Depends(get_db) counts as a call
  assert_eq!(pay.logs[0].level, "error"); assert_eq!(pay.logs[0].template, "payment failed for order %s"); }
#[test] fn env_reads_all_forms() { // os.getenv("A"), os.environ["B"], os.environ.get("C"), os.getenv("E", "dflt"), class S(BaseSettings): d_url: str  → "D_URL"
  assert_eq!(sorted(all_env_reads(&f)), ["A","B","C","D_URL","E?"]); } // trailing "?" = read has a default
#[test] fn worker_entrypoint_and_fstring_log() { let f = parse_file(Lang::Python, WORKER_PY);
  assert!(sym(&f,"run").entrypoint); assert_eq!(sym(&f,"run").logs[0].template, "retrying job {job_id}"); }
#[test] fn http_client_calls() { // in one fn: httpx.get(f"{BASE}/api/orders/{oid}") and requests.post(url + "/x")
  let c = &sym(&f, "fetch").http_calls; assert_eq!(c.len(), 1); assert_eq!((c[0].method.as_str(), c[0].path.as_str()), ("GET", "/api/orders/{}")); }
#[test] fn template_regex_matches_emitted_line() { let re = Regex::new(&template_regex("payment failed for order %s").unwrap()).unwrap();
  assert!(re.is_match("2026-10-24 12:00:01 ERROR app.main payment failed for order 42"));
  assert!(template_regex("{}").is_none()); }
#[test] fn pydantic_schema_and_annotations() { let f = parse_file(Lang::Python, MODELS_PY); assert_eq!(sym(&f, "PayRequest").kind, SymbolKind::Schema);
  let f = parse_file(Lang::Python, MAIN_PY); assert_eq!(sym(&f, "pay").uses_types, ["PayRequest"]); }
#[test] fn raises_and_handlers() { let f = parse_file(Lang::Python, MAIN_PY); let pay = sym(&f, "pay"); let h = &pay.handlers[0];
  assert_eq!(h.types, ["Exception"]); assert_eq!(h.action, HandlerAction::Reraise); assert_eq!((h.reraise_as.as_deref(), h.status), (Some("HTTPException"), Some(502)));
  let c = pay.calls.iter().find(|c| c.name == "charge").unwrap(); assert!(h.try_start <= c.line && c.line <= h.try_end);
  assert_eq!(sym(&f, "charge").raises[0].type_name, "PaymentGatewayError"); assert_eq!(sym(&f, "PaymentGatewayError").bases, ["Exception"]);
  assert_eq!(sym(&f, "on_value_error").handles_exception, Some(("ValueError".into(), Some(422)))); }
#[test] fn except_tuple_and_bare() { /* `except (A, B) as e: pass` → types [A, B], Swallow (as_pattern wrapping tuple); bare `except:` → types [] */ }
#[test] fn queue_ops_detected() { let pay = sym(&parse_file(Lang::Python, MAIN_PY), "pay"); assert_eq!((pay.queue_ops[0].queue.as_deref(), pay.queue_ops[0].op, pay.queue_ops[0].flavor), (Some("jobs:pay"), QueueDir::Send, Flavor::RedisList));
  let ro = sym(&parse_file(Lang::Python, MAIN_PY), "refund_order"); assert_eq!(ro.queue_ops[0].task_ref.as_deref(), Some("refund")); assert!(ro.queue_ops[0].queue.is_none());
  let w = parse_file(Lang::Python, WORKER_PY); assert_eq!(sym(&w, "run").queue_ops[0].op, QueueDir::Receive);
  let t = sym(&w, "refund"); assert_eq!(t.task.as_ref().unwrap().name, "orders.refund"); assert_eq!(t.task.as_ref().unwrap().autoretry_for, ["TimeoutError"]);
  /* f-string key f"jobs:{tenant}" → queue "jobs:{}"; non-literal key → no op */ }
#[test] fn db_ops_detected() { let f = parse_file(Lang::Python, MAIN_PY);
  assert!(sym(&f, "pay").db_ops.iter().any(|d| d.table.as_deref() == Some("payments") && d.access == Access::Write && d.via == Via::Sql));
  assert!(sym(&f, "charge").db_ops.iter().any(|d| d.model.as_deref() == Some("Order") && d.access == Access::Read && d.via == Via::Orm));
  assert_eq!(sym(&parse_file(Lang::Python, MODELS_PY), "Order").table.as_deref(), Some("orders"));
  /* "SELECT o.id FROM public.orders o JOIN payments p ON …" → reads public.orders and payments; a string that isn't SQL → none */ }
#[test] fn body_hash_ignores_formatting_and_comments() { let a = sym(&parse_file(Lang::Python, "def f(x):\n    return x + 1\n"), "f").body_hash.clone();
  let b = sym(&parse_file(Lang::Python, "def f(x):  # c\n\n    return   x + 1\n"), "f").body_hash.clone();
  let c = sym(&parse_file(Lang::Python, "def f(x):\n    return x + 2\n"), "f").body_hash.clone(); assert_eq!(a, b); assert_ne!(a, c); }
#[test] fn docstring_and_qualname() { let f = parse_file(Lang::Python, "class A:\n    def m(self):\n        \"\"\"Charge the card.\"\"\"\n        def inner(): pass\n");
  assert_eq!(sym(&f, "m").qualname, "A.m"); assert_eq!(sym(&f, "m").doc.as_deref(), Some("Charge the card.")); assert_eq!(sym(&f, "inner").qualname, "A.m.inner"); }
#[test] fn nested_and_duplicate_defs() { // def a(): def inner(): ...; and two `def prop` under @property/@prop.setter → 3 symbols, distinct start_line
}
```
- [ ] **Step 2: Run.** `cargo test -p engine parse::python`. Expected: FAIL.
- [ ] **Step 3: Implement `python::extract(src) -> FileFacts`** with a recursive walk over the tree-sitter tree. Add `sha2` to `[workspace.dependencies]` and engine, then run `cargo deny check`. `body_hash`: walk the symbol's leaf nodes, skip `comment`, join their text with one space, then sha256.
  - **Functions and methods:** `function_definition`. `owner` is the enclosing `class_definition`. Decorators come from the parent `decorated_definition`.
  - **Calls:** `call` → last segment of the `function` field. Arguments of `Depends(x)` / `Security(x)` count as calls to `x`.
  - **Logs:** callee is one of `debug info warning warn error exception critical` on a receiver named `log|logger|logging|_log|_logger|structlog` or one ending in `logger`, or loguru's `logger.*`. Take the first string arg. f-strings keep `{expr}` verbatim. Levels: `exception|critical` → `error`, `warning` → `warn`.
  - **Routes:**
    - Decorator `@<x>.get|post|put|patch|delete|api_route(path)`.
    - The router prefix comes from `<x> = APIRouter(prefix="…")` in the same file.
    - The include prefix comes from `app.include_router(<x>, prefix="…")` in the same file.
    - `// ponytail: same-file prefixes only; resolve cross-file include_router via imports if routes come out unprefixed`.
  - **HTTP calls:** `httpx|requests|client|session` `.get|post|put|patch|delete(...)` whose first arg is a string or f-string.
  - **Env reads:** `os.getenv("X")`, `os.environ["X"]`, `os.environ.get("X")`, and annotated fields of a class that subclasses `BaseSettings` (uppercased). A read with a default (a second arg, or a field with `= value`) is recorded as `"X?"`.
  - **Entrypoint:** functions called inside `if __name__ == "__main__":`, a function named `main`, and any function carrying a route.
  - **Flow facts** (flow design §1.1, §3.1, §4.1):
    - Raises come from `raise_statement`.
    - Handlers come from `try_statement` → `except_clause`. With `as`, the types sit inside `as_pattern` (a `tuple` for several), as verified with tree-sitter-python 0.25.
    - `bases` come from `class_definition.superclasses`.
    - Queue, task, SQL and ORM patterns are exactly the tables in the flow design.
    - Calls record their line.
- [ ] **Step 4: Run.** Expected: PASS.
- [ ] **Step 5: Commit.** `git commit -am "feat(engine): python extractor (fastapi, worker)"`

---

### Task 5: Angular extractor (TS, HTML, CSS)

**Files:**
- Create: `services/engine/src/parse/typescript.rs`, `services/engine/src/parse/html.rs`, `services/engine/src/parse/css.rs`

**Interfaces:**
- Consumes: the types in `parse/mod.rs` (Task 4)
- Produces: `typescript::extract`, `html::extract`, `css::extract`, all `(&str) -> FileFacts`, dispatched by `parse_file`.

- [ ] **Step 1: Failing tests**
```rust
#[test] fn component_and_injection() { let f = parse_file(Lang::TypeScript, COMPONENT_TS);
  let c = sym(&f, "OrdersComponent"); assert_eq!(c.kind, SymbolKind::Component);
  assert_eq!(c.selector.as_deref(), Some("app-orders")); assert_eq!(c.template_url.as_deref(), Some("./orders.component.html"));
  assert_eq!(c.injects, ["OrdersService"]); assert_eq!(sym(&f,"onPay").owner.as_deref(), Some("OrdersComponent")); }
#[test] fn inject_fn_form() { /* `private svc = inject(OrdersService)` → injects ["OrdersService"] */ }
#[test] fn httpclient_call_and_console_log() { let f = parse_file(Lang::TypeScript, SERVICE_TS); let pay = sym(&f, "pay");
  assert_eq!(pay.http_calls[0].method, "POST"); assert_eq!(pay.http_calls[0].path, "/api/orders/{}/pay");
  assert_eq!(pay.logs[0].template, "pay request failed"); assert_eq!(sym(&f,"OrdersService").kind, SymbolKind::Injectable); }
#[test] fn template_bindings() { let f = parse_file(Lang::Html, "<app-cart></app-cart><button (click)=\"onPay()\" (keyup.enter)=\"save($event)\">x</button>");
  let t = f.template.unwrap(); assert_eq!(t.bindings, [("click".into(),"onPay".into()), ("keyup.enter".into(),"save".into())]); assert_eq!(t.elements, ["app-cart"]); }
#[test] fn css_selectors() { assert_eq!(parse_file(Lang::Css, ".pay { color: red } #x .y:hover {}").css_selectors, [".pay", "#x .y:hover"]); }
#[test] fn ts_interfaces_are_schemas() { let f = parse_file(Lang::TypeScript, "export interface PayRequest { amount: number }\nexport type Id = string;\nclass S { pay(b: PayRequest): Observable<Order> {} }");
  assert_eq!(sym(&f, "PayRequest").kind, SymbolKind::Schema); assert_eq!(sym(&f, "pay").uses_types, ["PayRequest", "Order"]); }
#[test] fn ts_throw_and_catch() { /* `try { g(); } catch (e) { throw new PayError('x'); }` → handlers[0].types == [] with Reraise, reraise_as PayError, raises[0].type_name == "PayError"; `this.http.get('/a').pipe(catchError(...))` → a handler whose try range covers that call line, action Return */ }
#[test] fn ts_jsdoc_and_hash() { /* `/** Pays the order. */ pay(id) {...}` → doc "Pays the order."; reformatting the body keeps body_hash */ }
#[test] fn ts_env_reads() { /* environment.apiUrl → env_reads ["environment.apiUrl"]; process.env.FOO → ["FOO"] */ }
```
- [ ] **Step 2: Run.** `cargo test -p engine parse::`. Expected: FAIL for the new tests.
- [ ] **Step 3: Implement.**
  - **Grammars:** `tree_sitter_typescript::LANGUAGE_TYPESCRIPT` for both `.ts` and `.js`, `tree_sitter_html::LANGUAGE`, `tree_sitter_css::LANGUAGE`.
  - **TS symbols:**
    - `class_declaration`: a decorator `@Component({...})` makes it a `Component`. On an **exported** class, the decorator is a child of the enclosing `export_statement`, not of `class_declaration` (verified with tree-sitter-typescript 0.23.2), so check both places. Read `selector` and `templateUrl` from the decorator's object literal. `@Injectable` makes it an `Injectable`.
    - `method_definition` / `function_declaration` / an arrow function assigned to a `const` make Functions or Methods.
    - Constructor parameters with a type annotation, plus `inject(X)`, fill `injects`.
  - **TS HTTP calls:** `this.http.<verb>(arg)` or `<anything named http|httpClient>.<verb>`, where `arg` is a template string or string literal. Run the path through `normalize_path`.
  - **TS logs:** `console.error|warn|info|log|debug` (`log` → `info`) and `this.logger.<level>`.
  - **TS flow facts:** `throw_statement` with `new_expression` gives raises. `try_statement`/`catch_clause` gives handlers with `types: []`. A `catchError(...)` inside `.pipe(...)` gives a handler covering the piped call's line. Class `extends` gives `bases`. Calls record their line.
  - **HTML:** attributes named `(event)` whose value starts with an identifier followed by `(` give a binding. Tag names containing `-` give `elements`.
  - **CSS:** `rule_set` selectors text, whitespace-collapsed.
- [ ] **Step 4: Run.** Expected: PASS.
- [ ] **Step 5: Commit.** `git commit -am "feat(engine): angular ts/html/css extractor"`

---

### Task 5b: OpenAPI contracts

**Files:**
- Create: `services/engine/src/contract.rs`

**Interfaces:**
- Consumes: `spans::yaml`, `spans::json` (Task 2)
- Produces:
  - `contract::OpenApi { title: Option<String>, endpoints: Vec<(String /*METHOD*/, String /*path*/, usize /*line*/)>, schemas: Vec<(String, usize /*line*/)> }`. Lines come from `spans::yaml` or `spans::json`.
  - `contract::parse_openapi(src: &str) -> Result<OpenApi>`: YAML or JSON (try JSON first, then YAML), OpenAPI 3.x. Endpoints come from `paths.*.{get,post,put,patch,delete,head,options}`, with `servers[0].url`'s path prepended when it has one. Schemas are the keys of `components.schemas`.

- [ ] **Step 1: Failing tests**
```rust
#[test] fn parses_fixture() { let o = parse_openapi(ORDERS_OPENAPI).unwrap(); assert_eq!(o.endpoints.iter().map(|(m,p,_)| (m.as_str(), p.as_str())).collect::<Vec<_>>(), [("POST", "/api/orders/{order_id}/pay")]); assert!(o.endpoints[0].2 > 1); assert_eq!(o.schemas[0].0, "PayRequest"); }
#[test] fn server_base_path_and_json() { let o = parse_openapi(r#"{"openapi":"3.0.0","servers":[{"url":"https://x.io/v1"}],"paths":{"/a":{"get":{}}}}"#).unwrap(); assert_eq!((o.endpoints[0].0.as_str(), o.endpoints[0].1.as_str()), ("GET", "/v1/a")); }
#[test] fn rejects_non_openapi() { assert!(parse_openapi("services: {}").is_err()); }
```
- [ ] **Step 2: Run.** `cargo test -p engine contract`. Expected: FAIL.
- [ ] **Step 3: Implement** with `serde_json::Value` / `serde_norway::Value`. No OpenAPI crate.
- [ ] **Step 4: Run.** Expected: PASS.
- [ ] **Step 5: Commit.** `git commit -am "feat(engine): openapi contract parsing"`

---

### Task 6: Graph builder (pure)

**Files:**
- Create: `services/engine/src/graph.rs`, `services/engine/src/chunk.rs`
- Test: `services/engine/tests/graph.rs`

**Interfaces:**
- Consumes: `Ctx`, `docker::*`, `parse::*`
- Produces:
  - `graph::Graph { nodes: Vec<Node>, edges: Vec<Edge>, stats: Stats }`
  - `Node { label: &'static str, id: String, props: serde_json::Map<String, Value> }`
  - `Edge { rel: &'static str, from: String, from_label: &'static str, to: String, to_label: &'static str, props: serde_json::Map<String, Value> }`. Task 7 groups edges by `(rel, from_label, to_label)` for its `MATCH`.
  - `Stats { files_indexed: usize, skipped: Vec<Skipped>, excluded_dirs: usize, unresolved_calls: usize, unresolved_queue_ops: usize /* non-literal keys (flow design §3.1) plus Celery sends whose task_ref resolves nowhere */, chunks: usize, chunks_truncated: usize }`
  - `Skipped { path: String, size: u64, reason: SkipReason /* TooLarge | Minified | OverrideSkip | Unparseable */ }`. Minified means `*.min.js`/`*.min.css`, or an average line length over 500 chars.
  - `chunk::ChunkParams { max_tokens: usize, overlap_tokens: usize, max_chunks: usize, strategy: Strategy, enabled: bool }`, and `chunk::ChunkParams::resolve(cfg: &ChunkingCfg, model_max_seq: Option<usize>, remote: bool) -> ChunkParams`. `Auto` becomes `model_max_seq`, else `remote_max_tokens` for remote models, else 256 before Task 7c can ask the model.
  - `chunk::est_tokens(s: &str) -> usize = ceil(chars / 4)`
  - `chunk::Piece { ord: usize, start_line: usize, end_line: usize, text: String /* header + "\n" + body slice */ }`
  - `chunk::split(header: &str, body: &str, body_start_line: usize, statements: &[(usize, usize) /* line ranges of the body's top-level statements */], p: &ChunkParams) -> (Vec<Piece>, bool /* truncated */)`. This is the algorithm in layer design §4.5.1:
    1. Greedily pack statements into the budget `max_tokens - est_tokens(header)`.
    2. Start each new chunk with the previous chunk's trailing statements, up to `overlap_tokens`.
    3. A statement over the budget is split into line windows.
    4. `Lines` strategy uses windows only.
    5. When `enabled` is false: one piece, with the body cut to the budget.
    6. Stop at `max_chunks` and return `truncated = true`.
  - `graph::FileGraph { nodes: Vec<Node>, edges: Vec<Edge> }`. Edges are file-local only: `CONTAINS DEFINES HAS_METHOD LOGS READS_ENV HANDLED_BY`.
  - `graph::file_graph(ctx: &Ctx, unit: &str /* service or lib name */, relpath: &str, src: &str, chunks: &ChunkParams) -> Result<FileGraph, SkipReason>`. `Err` when the file is skipped. Size and minified checks happen before parsing, using `indexing::limit_for`. It also emits `:Chunk` nodes (`chunk:{sym_id}:{ord}`, `layer: "structural"`, with `ord`, `start_line`, `end_line`, a redacted `text`, `text_hash`, `tokens_est`) and `CHUNK_OF` edges for every symbol with a card. A truncated symbol gets `chunks_truncated: true`.
  - `graph::workspace_graph(ctx: &Ctx) -> Result<Graph>`: Service/Datastore/EnvVar/Contract/Endpoint/Schema-from-OpenAPI nodes, plus docker edges (DEPENDS_ON CONNECTS_TO SHARES_VOLUME SETS_ENV USES_DATASTORE PROVIDES DECLARES DEFINES_SCHEMA). No per-file parsing.
  - `graph::LINK_RELS: &[&str] = &["CALLS","CALLS_API","USES_SCHEMA","IMPLEMENTED_BY","INJECTS","TEMPLATE_BINDS","USES_LIB","CONSUMES","SENDS_TO","RECEIVES_FROM","READS_TABLE","WRITES_TABLE","MAPS_TO","ON","BOUNDARY"]` and `graph::LINK_LABELS: &[&str] = &["Queue","Table"]``
  - `graph::LinkOutput { nodes: Vec<Node> /* Queue and Table */, edges: Vec<Edge>, unresolved_calls: usize, unresolved_queue_ops: usize }`
  - `graph::link(ctx: &Ctx, nodes: &[Node], workspace_edges: &[Edge]) -> LinkOutput`: computes the LINK_LABELS nodes (`queue:{flavor}:{name}`, `table:{datastore|_}:{schema.}name`, per flow design §3.2 and §4.2) and every LINK_RELS edge.
    - `CALLS` edges carry `lines: [..]`.
    - **Celery sends:** `task_ref` resolves against a **workspace-wide** index of `task.name` and task function names, not through service-scoped call resolution. The task may live in another service.
    - **ORM access:** `DbOp.model` resolves to a class with `table` through the normal order (same file, same service, used libs).
    - **Table ids** use the accessing service's single Postgres datastore, else `_`. A library model's `MAPS_TO` points at the table node of each service that references it, so one table is one node.
    - `BOUNDARY.via` adds `queue` (producer service → consumer service) and `table` (two services touching one table).
    - Inputs: `queue_ops`, `db_ops`, `table` and `task`, plus from node properties (`calls`, `http_calls`, `uses_types`, `injects`, `template_url`, `imports` on File, `routes`, `qualname`, `unit`) plus `workspace_edges` (`DEPENDS_ON`, `CONNECTS_TO`, `SHARES_VOLUME` and `PROVIDES`, needed for `BOUNDARY` and `CONSUMES`). It never reads files. `BOUNDARY` is computed last, from the other link edges and `workspace_edges`.
  - `graph::build(ctx: &Ctx) -> Result<Graph>`: `workspace_graph` + every `file_graph` + `link`, with `ChunkParams::resolve(cfg, None, false)`. Used by `--dry-run` and tests, so dry-run chunk counts are provisional (256-token budget). `store::index` calls `file_graph` itself, with the resolved params.
  - `graph::card(node: &Node) -> Option<String>`: the card **header** for Function/Class/Component/Injectable/Schema/Route: kind, qualname, signature, doc, decorators/route, log templates, file path, joined by newlines and capped at 1 KiB. `None` for other labels. Every chunk of the symbol starts with this header.
  - Labels and rels are exactly those in the spec's graph model. In full:
    - Labels: `Service Datastore File Function Class Component Injectable Schema LogStatement Route EnvVar Contract Endpoint Chunk Queue Table`.
    - Rels: `SENDS_TO RECEIVES_FROM READS_TABLE WRITES_TABLE MAPS_TO ON CHUNK_OF DEPENDS_ON CONNECTS_TO SHARES_VOLUME USES_LIB USES_DATASTORE CONTAINS DEFINES HAS_METHOD CALLS LOGS HANDLED_BY CALLS_API INJECTS TEMPLATE_BINDS READS_ENV SETS_ENV PROVIDES CONSUMES DECLARES IMPLEMENTED_BY DEFINES_SCHEMA USES_SCHEMA BOUNDARY`.
    - Every node gets `props.workspace = ctx.config.workspace.name`.
  - **Node ids:**
    - `svc:{name}`, `ds:{name}`, `env:{NAME}`
    - `file:{svc}:{relpath}`
    - `sym:{svc}:{relpath}:{qualname}`, plus `~2`, `~3`, … for repeats of the same qualname in one file, in source order. No line numbers.
    - `log:{sym_id}:{n}`, where `n` is the statement's order within the function.
    - `route:{svc}:{METHOD}:{path}`
  - More node ids: `contract:{name}`, `endpoint:{contract}:{METHOD}:{path}`, `schema:{contract}:{name}`.
  - Every structural node has `layer: "structural"`. File nodes carry `hash` (sha256 of the content), `size` and `imports`. Symbol nodes carry `qualname`, `body_hash`, `start_line`, `end_line`, `doc`, `card`, the link facts listed under `link`, and the flow facts Part 2 Task 10b reads without re-parsing (flow design §7): `raises`, `handlers`, `bases`, `handles_exception` and `task`, as JSON-valued properties.
  - `LogStatement.props` holds `regex` (from `template_regex`), `service` and `file`, so Part 2 can match without joins.

- [ ] **Step 1: Failing tests** (against the Task 3 fixture with its generated config)
```rust
#[test] fn service_edges() { let g = build(&ctx()).unwrap();
  assert!(has_edge(&g, "DEPENDS_ON", "svc:orders", "svc:db")); assert!(has_edge(&g, "CONNECTS_TO", "svc:orders", "svc:cache"));
  assert!(has_edge(&g, "SHARES_VOLUME", "svc:orders", "svc:worker")); assert!(has_edge(&g, "USES_LIB", "svc:orders", "svc:common"));
  assert!(has_edge(&g, "USES_DATASTORE", "svc:orders", "ds:db")); }
#[test] fn code_edges() { let g = build(&ctx()).unwrap(); let pay = id_of(&g, "Function", "pay"); let charge = id_of(&g, "Function", "charge");
  assert!(has_edge(&g, "CALLS", &pay, &charge)); assert!(has_edge(&g, "CALLS", &pay, &id_of(&g, "Function", "get_db"))); // cross-service into lib
  assert!(has_edge(&g, "HANDLED_BY", "route:orders:POST:/api/orders/{order_id}/pay", &pay));
  assert!(has_edge(&g, "READS_ENV", &charge, "env:PAYMENT_GATEWAY_URL")); assert!(has_edge(&g, "SETS_ENV", "svc:orders", "env:DATABASE_URL")); }
#[test] fn queue_and_table_edges() { let g = build(&ctx()).unwrap(); let q = "queue:redis_list:jobs:pay";
  assert!(has_edge(&g, "SENDS_TO", &id_of(&g, "Function", "pay"), q) && has_edge(&g, "RECEIVES_FROM", &id_of(&g, "Function", "run"), q));
  assert!(has_edge(&g, "SENDS_TO", &id_of(&g, "Function", "refund_order"), "queue:celery:orders.refund") && has_edge(&g, "RECEIVES_FROM", &id_of(&g, "Function", "refund"), "queue:celery:orders.refund"));
  assert!(has_edge(&g, "WRITES_TABLE", &id_of(&g, "Function", "pay"), "table:db:payments") && has_edge(&g, "MAPS_TO", &id_of(&g, "Class", "Order"), "table:db:orders"));
  assert!(has_edge(&g, "READS_TABLE", &id_of(&g, "Function", "charge"), "table:db:orders"));
  assert!(via(edge(&g, "BOUNDARY", "svc:orders", "svc:worker").unwrap()).contains(&"queue"));
  assert!(edge_props(&g, "CALLS", &id_of(&g, "Function", "pay"), &id_of(&g, "Function", "charge"))["lines"].as_array().unwrap().len() == 1); }
#[test] fn frontend_to_backend() { let g = build(&ctx()).unwrap();
  assert!(has_edge(&g, "CALLS_API", &id_of(&g, "Function", "pay@OrdersService"), "route:orders:POST:/api/orders/{order_id}/pay"));
  assert!(has_edge(&g, "INJECTS", &id_of(&g, "Component", "OrdersComponent"), &id_of(&g, "Injectable", "OrdersService")));
  assert!(has_edge(&g, "TEMPLATE_BINDS", &id_of(&g, "Component", "OrdersComponent"), &id_of(&g, "Function", "onPay"))); }
#[test] fn contracts_and_boundaries() { let g = build(&ctx()).unwrap();
  assert!(has_edge(&g, "PROVIDES", "svc:orders", "contract:orders-openapi"));
  assert!(has_edge(&g, "IMPLEMENTED_BY", "endpoint:orders-openapi:POST:/api/orders/{order_id}/pay", "route:orders:POST:/api/orders/{order_id}/pay"));
  assert!(has_edge(&g, "CONSUMES", "svc:frontend", "contract:orders-openapi")); // via CALLS_API into an implemented endpoint
  assert!(has_edge(&g, "DEFINES_SCHEMA", "contract:common", &schema_id(&g, "PayRequest", "common"))); // derived library contract
  assert!(has_edge(&g, "CONSUMES", "svc:worker", "contract:common")); // imports a schema from the library
  assert!(has_edge(&g, "USES_SCHEMA", &id_of(&g, "Function", "pay"), &schema_id(&g, "PayRequest", "common")));
  let b = edge(&g, "BOUNDARY", "svc:frontend", "svc:orders").unwrap(); assert!(via(b).contains(&"http") && via(b).contains(&"contract"));
  let b = edge(&g, "BOUNDARY", "svc:orders", "svc:worker").unwrap(); assert!(via(b).contains(&"compose") && via(b).contains(&"shared_lib")); }
#[test] fn ids_have_no_lines() { let g = build(&ctx()).unwrap(); assert!(g.nodes.iter().filter(|n| n.id.starts_with("sym:")).all(|n| !n.id.rsplit(':').next().unwrap().chars().all(|c| c.is_ascii_digit()))); }
#[test] fn link_from_props_matches_build() { let g = build(&ctx()).unwrap(); let ws: Vec<_> = g.edges.iter().filter(|e| !LINK_RELS.contains(&e.rel)).cloned().collect(); let mut l = link(&ctx(), &g.nodes, &ws).edges;
  let mut b: Vec<_> = g.edges.iter().filter(|e| LINK_RELS.contains(&e.rel)).cloned().collect(); l.sort_by(|a,b| key(a).cmp(&key(b))); b.sort_by(|a,b| key(a).cmp(&key(b))); assert_eq!(keys(&l), keys(&b)); }
#[test] fn card_contents() { let g = build(&ctx()).unwrap(); let c = card(node(&g, &id_of(&g, "Function", "pay"))).unwrap();
  assert!(c.contains("POST /api/orders/{order_id}/pay") && c.contains("payment failed for order %s") && c.contains("app/main.py")); }
#[test] fn ids_are_unique() { let g = build(&ctx()).unwrap(); let mut ids: Vec<_> = g.nodes.iter().map(|n| &n.id).collect(); let n = ids.len(); ids.sort(); ids.dedup(); assert_eq!(ids.len(), n); }
#[test] fn build_skips_excluded_and_large() { /* add a 2 MiB .py and a foo.min.js to the tmp fixture; node_modules/x/index.js exists */
  let g = build(&ctx()).unwrap(); assert!(g.nodes.iter().all(|n| !n.id.contains("node_modules") && !n.id.contains("min.js") && !n.id.contains("big.py")));
  assert!(g.stats.skipped.iter().any(|s| s.path.ends_with("big.py") && s.reason == SkipReason::TooLarge)); assert!(g.stats.skipped.iter().any(|s| s.reason == SkipReason::Minified)); }
#[test] fn override_raises_limit() { /* same tmp fixture with indexing.overrides [{path:"**/big.py", max_file_bytes:"4MiB"}] → big.py is indexed, not in skipped */ }
#[test] fn long_function_chunks() { /* reconcile_refunds from the fixture, ChunkParams{max_tokens:256, overlap:32, max_chunks:32, Syntax}:
  pieces.len() > 1; every piece starts with the header; every piece's est_tokens <= 256; consecutive pieces overlap (next.start_line <= prev.end_line);
  the union of line ranges covers the whole body; max_chunks 2 → 2 pieces and truncated == true */ }
#[test] fn oversized_statement_falls_back_to_lines() { /* one 3k-char statement → several pieces, each within budget */ }
#[test] fn short_symbol_one_chunk() { let g = build(&ctx()).unwrap(); assert_eq!(chunks_of(&g, &id_of(&g, "Function", "charge")).len(), 1); }
#[test] fn deterministic() { assert_eq!(serde_json::to_string(&build(&ctx()).unwrap().nodes).unwrap(), serde_json::to_string(&build(&ctx()).unwrap().nodes).unwrap()); }
```
(`id_of(g, label, "name@Owner")` is a test helper that finds the node with that label, name and optional owner.)
- [ ] **Step 2: Run.** `cargo test -p engine --test graph`. Expected: FAIL.
- [ ] **Step 3: Implement `build`.**
  - **Service nodes:** one per config service and shared lib, plus one per compose service not linked to config (`kind: infra`, `image`).
  - **Datastore nodes:** from config.
  - **Docker edges:** DEPENDS_ON, CONNECTS_TO, SHARES_VOLUME and SETS_ENV from compose (inline env, env_file and Dockerfile ENV; props `source`, `file`, `line`). Compose names map to config service names.
  - **Files:** walk each service/lib path with `walkdir`, skipping `exclude` dirs (counted) and files where `Lang::from_path` is `None`. Size, minified and override checks happen in `file_graph`, and their skips land in `Stats.skipped`.
  - **Call resolution, in this order:**
    1. Same file.
    2. Same service.
    3. Libs in the caller's `uses` plus libs whose name (with `-`/`_` normalized) appears in one of the file's imports.
    - Unresolved → `stats.unresolved_calls += 1`.
    - `// ponytail: name-based resolution; ambiguous names link to the first match; use LSP definitions when precision matters`.
  - **CALLS_API:** match the normalized `HttpCall.path` against every Route path normalized the same way (`{x}` → `{}`). When `api.prefix` is set on the target service, also try `prefix + path`. Method must match. Prefer the longest path that equals the call's path or is a suffix of it.
  - **TEMPLATE_BINDS:** a component's `template_url` resolved relative to its file.
  - **INJECTS:** by class name across the frontend service.
  - **Contracts:**
    - An `openapi` contract (named after its file: `orders/openapi.yaml` → `orders-openapi` unless config names it) gives Endpoint and Schema nodes from `contract::parse_openapi`, PROVIDES from `provider`, IMPLEMENTED_BY where an Endpoint and a provider Route share method and normalized path, and CONSUMES for config `consumers` plus every service with a CALLS_API edge into an implemented Route.
    - A `library` contract is derived for every shared lib that defines at least one `Schema` symbol (config can also declare one). Its Schema nodes are those symbols. CONSUMES/PROVIDES: every service importing a schema from the lib CONSUMES it; the config `provider` PROVIDES it.
    - USES_SCHEMA: a function's `uses_types` resolved first against schemas in its own service, then in libs it imports, then in openapi contracts it consumes.
  - **BOUNDARY** (last): for each unordered service pair, collect `via` from compose (DEPENDS_ON/CONNECTS_TO/SHARES_VOLUME), `http` (CALLS_API across services), `contract` (one PROVIDES and the other CONSUMES the same contract), and `shared_lib` (both USES_LIB the same lib). Direction is consumer → provider when known, else sorted by name. `evidence` = the ids of the underlying edges, as `"{rel}:{from}->{to}"`.
  - Sort nodes and edges by id before returning.
- [ ] **Step 4: Run.** Expected: PASS.
- [ ] **Step 5: Commit.** `git commit -am "feat(engine): graph builder"`

---

### Task 7: Memgraph store with incremental indexing, `ops index`, `ops check`

**Files:**
- Create: `services/engine/src/store.rs`, `services/engine/src/lock.rs`
- Modify: `services/cli/src/main.rs`

**Interfaces:**
- Consumes: `Ctx`, `graph::{file_graph, workspace_graph, link, LINK_RELS, Graph, Node, Edge}`
- Produces (Tasks 7b–7d and Part 2 depend on these):
  - Low level:
    - `store::query(ctx: &Ctx, cypher: &str, params: serde_json::Map<String, Value>) -> Result<Vec<serde_json::Map<String, Value>>>`: synchronous; one map per record, keyed by column. Node → `{"_labels": [...], ...props}` (with `embedding` removed), Relationship → `{"_type": ..., ...props}`, Path → `{"nodes": [...], "rels": [...]}`.
    - `store::query_async(ctx: Ctx, cypher: String, params) -> Result<…>`: `spawn_blocking` around `query`, connecting inside the closure (rsmgclient's `Connection` is not `Send`).
    - `store::ping(ctx: &Ctx) -> Result<()>`
    - `store::ensure_indexes(ctx: &Ctx, labels: &[&str]) -> Result<()>`: **autocommit**, outside any transaction. Creates `:<Label>(id)` and `:<Label>(workspace)`. Memgraph rejects index changes inside multi-statement transactions (`MulticommandTxException`: "… is not allowed in multicommand transactions", verified at v3.14.0). "Already exists" errors are ignored.
  - `lock::IndexLock::acquire(ctx: &Ctx, wait: Duration) -> Result<IndexLock>`: takes `.ops/index.lock`, holding `{pid, started_at}`. A lock whose pid is dead is taken over. Returns `Err("index in progress (pid N, since T)")` once `wait` runs out. The lock is released on drop.
  - `enum IndexMode { Incremental, Full }`
  - `store::IndexReport { mode, added, changed, deleted, unchanged, nodes, edges, unresolved_calls, unresolved_queue_ops, fingerprint_changed: bool, vanished: Vec<VanishedAnchor>, run_id: String, skipped: Vec<Skipped>, chunks: usize, chunks_truncated: usize, embedding: String /* Task 7c: "vector", or "text-only: <reason>" */, embedded: usize /* Task 7c: chunks embedded this run */ }`. Until Task 7c lands, `embedding` is `"off"` and `embedded` is 0.
  - The signature of `store::index` is unchanged. `--max-file-bytes` is applied by the CLI before the call, by setting `ctx.config.indexing.max_file_bytes = EnvOr::Literal(size)` on its loaded `Ctx`. Precedence: flag, then `OPS_INDEXING__MAX_FILE_BYTES` (through the `config` crate), then `ops.yaml`.
  - `store::VanishedAnchor { knowledge_id: String, rel: String /* ABOUT|STEP|FROM|TO */, anchor_key: String, anchor_hash: String, qualname: Option<String>, unit: Option<String> }`
  - `store::index(ctx: &Ctx, mode: IndexMode, wait: Duration) -> Result<IndexReport>`. The algorithm is the layer design's §3:
    1. **Lock.** Take the lock, then run `ensure_indexes`.
    2. **Fingerprint.** `parse_fingerprint = sha256(ops version + canonical JSON of config.services, shared_libs, contracts, exclude, docker, the effective indexing section, and the resolved ChunkParams)`. If it differs from the latest `IndexRun.parse_fingerprint`, mode becomes Full.
    3. **Plan.** Walk the files, then `MATCH (f:File {workspace:$ws, layer:'structural'}) RETURN f.path, f.hash`, and diff the two.
    4. **One transaction:**
       - a. For each added or changed file: `file_graph`, then `UNWIND … MERGE (n:<Label> {id:r.id}) SET n += r` per label. `r` carries **every** structural key, with null for absent ones, which removes them. `+=` keeps `embedding` and `card_hash`, which Task 7c owns; `SET n = r` would erase them.
         - Delete that file's old local edges (`CONTAINS DEFINES HAS_METHOD LOGS READS_ENV HANDLED_BY`) and create the new ones.
         - Collect the `(label, id)` pairs the file produced before but no longer does. Pairs, not bare ids: a symbol that changes kind (a Class gaining `BaseModel` becomes a Schema) keeps its id but changes label, so the old `:Class` node must count as vanished.
       - b. Deleted files: all their `(label, id)` pairs.
       - c. Workspace graph: MERGE its nodes (`SET n += r`, same rule). Delete structural nodes of labels Service/Datastore/EnvVar/Contract/Endpoint/Schema(openapi) that it no longer produces. Replace its edges.
       - d. **Before deleting** the vanished nodes: `MATCH (k:Knowledge)-[r]->(n)` over the vanished `(label, id)` pairs → `VanishedAnchor`s. Then `DETACH DELETE` those nodes, matching `layer:'structural'`.
       - d2. **Still inside the transaction**, after step e: `semantic::reattach_vanished(tx, &vanished)` (Task 7d). It re-attaches by qualname or records orphans, so a crash can never leave anchors cut while entries still read `current`. Until Task 7d lands, this is a no-op that returns the list unchanged.
       - e. Relink: delete every edge whose type is in LINK_RELS within the workspace. Load all symbol, route and file nodes (props only) and the workspace edges (`DEPENDS_ON CONNECTS_TO SHARES_VOLUME PROVIDES`), run `graph::link(ctx, nodes, edges)`. MERGE its Queue/Table nodes (`SET n += r`); for link nodes no longer produced (by `(label, id)`), collect their incoming semantic anchors exactly as in step d, add them to `vanished`, then delete them. Then create its edges.
       - f. Write `(:IndexRun {id, started_at, finished_at, mode, files_added, files_changed, files_deleted, parse_fingerprint, embed_fingerprint, git_head, workspace, layer:'structural'})`. Keep the newest 20.
    5. **Commit.** On any error, roll back. The previous graph stays intact.

    Full mode is the same steps with every file treated as changed, plus: structural nodes not produced by this run are deleted (through step d). The structural layer is never wiped wholesale.
  - `IndexReport` is also what Task 7d's reconcile consumes (`vanished`, `run_id`).
  - **Two fingerprints** on `IndexRun`:
    - `parse_fingerprint` (ops version + config sections): a change forces a full re-parse.
    - `embed_fingerprint` (Task 7c): a change forces only a vector rebuild and re-embed, with no re-parse.

- [ ] **Step 1: Read the rsmgclient 4.0 API before coding.** Look in `~/.cargo/registry/src/*/rsmgclient-4.0.0/src/` and confirm:
  - `ConnectParams` (host, port, username, password, autocommit)
  - `Connection::connect`, `execute`, `fetchall`, `commit`, `rollback`
  - the `QueryParam` and `Value` variants

  Write the JSON ↔ `QueryParam`/`Value` converters to match what you find. (Verified in 4.0.0: `QueryParam` has `Null Bool Int Float String List Map` plus temporal and point types; `ConnectParams` has `autocommit`; `Connection` has `rollback`.) Call `Connection::init()` once per process before the first connect.
- [ ] **Step 2: Failing tests**
```rust
#[test] fn json_param_round_trip() { /* json!({"a":[1,"x",{"b":true}],"n":null}) → QueryParam::Map(...) with nested List/Map/Null (pure) */ }
#[test] fn lock_blocks_second_indexer() { /* acquire on tmp ctx; second acquire with wait 0 → Err containing "index in progress"; drop first → second succeeds; stale lock with dead pid is taken over */ }
#[test] #[ignore] fn index_twice_same_counts() { let ctx = tmp_fixture_ctx(); let a = store::index(&ctx, IndexMode::Full, Duration::ZERO).unwrap();
  let b = store::index(&ctx, IndexMode::Incremental, Duration::ZERO).unwrap(); assert_eq!((b.added, b.changed, b.deleted), (0, 0, 0)); assert_eq!((a.nodes, a.edges), (b.nodes, b.edges)); }
#[test] #[ignore] fn incremental_equals_full() { let ctx = tmp_fixture_ctx(); store::index(&ctx, IndexMode::Full, Duration::ZERO).unwrap();
  edit(&ctx, "services/orders/app/main.py", |s| s.replace("def charge(", "def charge_card(").replace("charge(order_id)", "charge_card(order_id)"));
  let r = store::index(&ctx, IndexMode::Incremental, Duration::ZERO).unwrap(); assert_eq!(r.changed, 1);
  let inc = dump(&ctx); store::index(&ctx, IndexMode::Full, Duration::ZERO).unwrap(); assert_eq!(inc, dump(&ctx)); } // dump = sorted node ids+props and edge keys, excluding IndexRun
#[test] #[ignore] fn raising_limit_picks_up_skipped() { /* 2 MiB file skipped on run 1; run 2 with ctx.config.indexing.max_file_bytes = 4MiB → fingerprint_changed, the file is indexed */ }
#[test] #[ignore] fn kind_change_replaces_node() { /* make a fixture class subclass BaseModel → after an incremental run, exactly one node with that id exists, labelled Schema; the incremental graph equals the --full graph */ }
#[test] #[ignore] fn deleted_file_removed() { /* delete services/worker/worker.py → its Function nodes and CALLS edges are gone; r.deleted == 1 */ }
#[test] #[ignore] fn fingerprint_forces_reparse() { /* change a service's framework in ops.yaml → next incremental run reports fingerprint_changed and changed == every file */ }
#[test] #[ignore] fn failed_run_keeps_previous_graph() { /* inject a failure after step 4a (test hook) → node count equals the count before the run */ }
```
- [ ] **Step 3: Run.** `cargo test -p engine store lock`. Expected: FAIL (compile).
- [ ] **Step 4: Implement** `store`, `lock`.
  - Credentials come from `memgraph.username`/`password` through `Secret::resolve`; when unset, connect without auth.
  - Error format is exactly `memgraph <host>:<port>: <redacted cause>`, never the password. Part 2's smoke test matches on the `memgraph ` prefix.
  - Batches of 500 rows per `UNWIND`.
  - `// ponytail: relink recomputes all link edges each run; scope it to the affected units if relink time shows up in profiles.`
- [ ] **Step 5: CLI commands.**
  - `ops index [--root] [--full] [--dry-run] [--wait SECS] [--max-file-bytes SIZE] [--show-skipped]`:
    - It prints the 20 largest skipped files with size and reason, plus the total; `--show-skipped` prints all of them. It also prints `chunks` and `chunks_truncated`.
    - `--dry-run` runs `graph::build` and prints `Stats` and per-label counts, without touching Memgraph.
    - Otherwise it runs `store::index` and prints the `IndexReport` as one line plus per-label counts.
  - `ops check [--root]`:
    1. `config::validate` problems.
    2. Every secret or `EnvOr` that doesn't resolve, named by its env var.
    3. `store::ping`.
    4. The last `IndexRun` time, the number of files changed since, and the number of files the last run skipped (layer design §3.1).
    5. `kubectl --context <ctx> version --client=false` when kubernetes is configured.
    6. Exit 1 if anything failed.

    Later tasks add: the embedding status and vector-index bloat (Task 7c), the export-age warning (Task 7d), git availability (Part 2 Task 11b), and datastore pings (Part 2 Task 12).
- [ ] **Step 6: Run.** `cargo test -p engine` (PASS). `cargo run -p cli -- index --dry-run --root services/engine/tests/fixtures/shop` (prints counts). With Docker up: `cargo test -p engine -- --ignored` (PASS).
- [ ] **Step 7: Commit.** `git commit -am "feat: incremental memgraph store, ops index, ops check"`

---

### Task 7b: `ops watch`

**Files:**
- Create: `services/engine/src/watch.rs`
- Modify: `services/cli/src/main.rs`, root `Cargo.toml` (add `notify`)

**Interfaces:**
- Consumes: `store::index`, `lock::IndexLock`, `config` `exclude`
- Produces:
  - `watch::Debouncer::new(window: Duration)`, with `push(path: PathBuf, at: Instant)` and `ready(now: Instant) -> Option<Vec<PathBuf>>`. It's pure: a batch is ready once `window` has passed with no new event.
  - `watch::run(ctx: &Ctx, debounce: Duration) -> Result<()>`: `notify` recursive watcher on the workspace root. It ignores excluded dirs, `.ops/`, `docker/` and `ops-knowledge/`. On each ready batch it calls `store::index(Incremental, wait = 30s)` and logs one summary line (`+a ~c -d in Nms`). A lock timeout is logged and the batch is retried on the next event.
  - CLI: `ops watch [--root] [--debounce-ms 1500]`.

- [ ] **Step 1: Failing tests**
```rust
#[test] fn debounce_batches() { let mut d = Debouncer::new(ms(1500)); d.push("a".into(), t(0)); d.push("b".into(), t(1000));
  assert!(d.ready(t(2000)).is_none()); assert_eq!(d.ready(t(2600)).unwrap(), ["a","b"]); assert!(d.ready(t(5000)).is_none()); }
#[test] fn ignores_excluded() { /* is_watched(ctx, "frontend/node_modules/x.js") == false; is_watched(ctx, "ops-knowledge/k.json") == false; "services/orders/app/main.py" == true */ }
```
- [ ] **Step 2: Run.** `cargo test -p engine watch`. Expected: FAIL.
- [ ] **Step 3: Implement.** Add `notify` (latest 8.x) to the workspace and run `cargo deny check`.
- [ ] **Step 4: Run.** Tests PASS. Manual check with Docker: `ops watch --root <tmp fixture>`, edit `main.py`, and a `~1` summary line appears within 2 s.
- [ ] **Step 5: Commit.** `git commit -am "feat: ops watch"`

---

### Task 7c: Embeddings, vector and text indexes

**Files:**
- Create: `services/engine/src/embed.rs`
- Modify: `services/engine/src/store.rs` (embedding step + fingerprint), `services/cli/src/main.rs` (`ops check` status)

**Interfaces:**
- Consumes: `store::{query, ensure_indexes}`, `EmbeddingsCfg`, `chunk::ChunkParams`, `:Chunk` nodes from Task 6
- Produces:
  - `embed::config_map(ctx: &Ctx, input_type: InputKind /* Document | Query */) -> Result<serde_json::Map>`: the module's configuration map.
    - `model` → `model_name`. `query_input_type` is used when `Query`, sent as `input_type`.
    - Null fields are omitted. `metric`, `scalar_kind`, `enabled` and `query_input_type` are never sent.
  - `embed::ModelInfo { dimension: usize, remote_provider: Option<String>, max_sequence_length: Option<usize> }`, and `embed::model_info(ctx) -> Result<ModelInfo>` via `CALL embeddings.model_info($cfg)`.
  - `enum EmbedStatus { Vector(ModelInfo), TextOnly(String /* reason */) }`, and `embed::status(ctx) -> EmbedStatus`. Gives `TextOnly` when `enabled` is false, the `embeddings` module is absent (`CALL mg.procedures()` has no `embeddings.text`), or `model_info` fails.
  - `embed::embed_texts(ctx, texts: &[String], kind: InputKind) -> Result<Vec<Vec<f32>>>` via `CALL embeddings.text($texts, $cfg)`, in chunks of 256.
  - `embed::ensure_search_indexes(ctx, info: Option<&ModelInfo>, rebuild_index: bool) -> Result<()>`. Autocommit.
    - Text indexes: `knowledge_text ON :Knowledge(title, body)`, `chunk_text ON :Chunk(text)`, and `card_text_<label> ON :<Label>(card)` per code label. The grammar takes one label plus an optional property list.
    - Vector index, when `info` is Some: a single `chunk_vec ON :Chunk(embedding)`, with `{dimension, capacity: max(1024, 2 × chunk count), metric, scalar_kind}`. There are no per-symbol or per-entry vectors.
    - Sets `omit_vector_index_properties_on_return` so vectors never come back from queries (layer design §4.5). It may be a server flag rather than a per-index option: confirm the name and where it goes on 3.14.0 (`memgraph --help`, `SHOW CONFIG`), and if it's a flag, add it to Task 0's `command`. `store::query` strips `embedding` either way.
    - `rebuild_index` drops and recreates the vector index. Dropping moves the stored vectors back into the property store (Memgraph docs), so recreating re-indexes the existing `embedding` properties **without** re-embedding.
  - **In `store::index`**, after relink and outside the main transaction (vector indexes are READ_UNCOMMITTED):
    - **`embed_fingerprint`** = sha256 of `model`, `dimensions`, `metric` and `scalar_kind`. A change sets `rebuild_index` **and** `reembed_all`, so it re-embeds everything, including `:Knowledge`. It does **not** trigger a re-parse.
    - **Resolve chunk parameters** with `ChunkParams::resolve(cfg, info.max_sequence_length, info.remote_provider.is_some())` **before** parsing. Pass them to `file_graph`; they're in `parse_fingerprint`.
    - **No flapping:** `IndexRun` stores the resolved `max_tokens`. When `model_info` is unavailable (transient error, text-only run), `Auto` reuses the last run's stored value instead of the 256 fallback, so a hiccup never forces a full re-parse. The fallback applies only when no run has stored a value yet.
    - **Re-embed only what changed:** `:Chunk` nodes with no `embedding`, or whose `text_hash` differs from their `embedded_hash`, plus every chunk when `reembed_all` is set. `SET c.embedding = $vec, c.embedded_hash = c.text_hash`. Re-embedding has exactly two triggers: a changed `embed_fingerprint`, or a changed chunk `text_hash`. A Full run never re-embeds by itself, which matters for paid remote models.
    - **`--full`** always sets `rebuild_index` (but not `reembed_all`), because deleted nodes don't free vector-index memory until the index is dropped.
    - **`TextOnly`:** skip embedding, and record the reason in `IndexReport.embedding: String`.
  - `ops check` prints `embeddings: vector (local all-MiniLM-L6-v2, dim 384)`, or `remote openai, dim 1536`, or `text-only: <reason>`. It also warns when `SHOW VECTOR INDEX INFO` reports more than 2× as many vectors as live nodes.

- [ ] **Step 1: Failing tests**
```rust
#[test] fn config_map_mapping() { let m = config_map(&ctx_with(r#"semantic: {embeddings: {model: "voyage/voyage-3", dimensions: 512}}"#), InputKind::Query).unwrap();
  assert_eq!(m["model_name"], "voyage/voyage-3"); assert_eq!(m["input_type"], "query"); assert_eq!(m["dimensions"], 512); assert!(!m.contains_key("metric") && !m.contains_key("api_base")); }
#[test] fn disabled_is_text_only() { assert!(matches!(status(&ctx_with("semantic: {embeddings: {enabled: false}}")), EmbedStatus::TextOnly(_)));
  /* no `semantic:` section, OPS_SEMANTIC__EMBEDDINGS__ENABLED=false set for the test → TextOnly as well (the core CI job relies on this; needs config's try_parsing) */ }
#[test] #[ignore] fn embed_chunks_default_model() { /* MAGE image: index fixture → every :Chunk has `embedding` of length 384 and ChunkParams.max_tokens resolved to 256 (all-MiniLM-L6-v2's max_sequence_length);
  second incremental run re-embeds 0; editing one statement in `reconcile_refunds` re-embeds no chunk that lies wholly before that statement. Greedy packing may shift every later boundary, so later chunks can re-embed; that's expected. */ }
#[test] #[ignore] fn embed_long_function_found_by_its_tail() { /* fixture function of ~200 lines whose last block mentions "refund ledger"; search over chunk_vec for "refund ledger reconciliation" ranks that function first (Part 2 search uses the same query) */ }
#[test] #[ignore] fn embed_full_run_reembeds_nothing() { /* index (embeds all) → `ops index --full` with the same model → report.embedded == 0 and every chunk still has its embedding; vector search still returns hits */ }
#[test] #[ignore] fn embed_model_info_failure_does_not_reparse() { /* index once (max_tokens resolved 256 from the model); make model_info fail (test hook) → next incremental run: fingerprint_changed false, changed == 0, warning about text-only */ }
#[test] #[ignore] fn embed_model_change_rebuilds() { /* change model to another local model with a different dim → vector index recreated with the new dimension; report.changed == 0 (no re-parse) */ }
```
- [ ] **Step 2: Run.** `cargo test -p engine embed`. Expected: FAIL.
- [ ] **Step 3: Implement.** Before coding, confirm the procedure signatures with `CALL mg.procedures() YIELD name, signature WHERE name STARTS WITH 'embeddings' OR name STARTS WITH 'vector_search' OR name STARTS WITH 'text_search' RETURN *`.
- [ ] **Step 4: Run.** Unit tests PASS. With `docker compose --profile embeddings up -d`, the `--ignored` tests PASS.
- [ ] **Step 5: Commit.** `git commit -am "feat: code-card embeddings and search indexes"`

---

### Task 7d: Semantic-layer core, drift reconciliation, export and import

**Files:**
- Create: `services/engine/src/semantic.rs`
- Modify: `services/engine/src/store.rs` (call `reattach_vanished` at step d2, inside the transaction, and `reconcile` after commit), `services/cli/src/main.rs` (`ops knowledge`), root `Cargo.toml` (add `uuid` with `v4`)

**Interfaces:**
- Consumes: `store::{query, IndexReport, VanishedAnchor}`, `embed::embed_texts`, `redact`
- Produces (Part 2 Task 8b wraps these as MCP tools):
  - `semantic::Kind { Summary, Concept, Flow, Relation, Invariant, Gotcha, OpsNote }` (serde snake_case), `Confidence { High, Medium, Low }`, `Status { Current, Stale, Orphaned, Retracted }`
  - `semantic::Entry { id, kind, title, body, confidence, status, evidence: Vec<String>, tags: Vec<String>, author_client: String, author_model: Option<String>, session_id: String, created_at, updated_at, anchors: Vec<Anchor>, steps: Vec<Anchor> /* flow */, from: Vec<Anchor>, to: Vec<Anchor> /* relation */, orphaned_anchors: Vec<String>, stale_since: Option<String> }`
  - `semantic::Anchor { node_id: String, anchor_key: String /* = node_id */, anchor_hash: String }`
  - `semantic::anchor_hash(node: &serde_json::Map) -> String`: the layer design's §4.3 table. Function/Class/Component/Injectable/Schema → `body_hash`; File → `hash`; Route → its handler's `body_hash`; Contract → its document hash; everything else → `""` (never stale).
  - `semantic::NewEntry { kind, title, body, confidence, evidence, tags, anchors: Vec<String> /* ids */, steps, from, to, author_client, author_model, session_id }`, and `semantic::create(ctx, NewEntry) -> Result<String /* "k:" + uuid v4 */>`.
    - Validates: title ≤ 120 chars, body ≤ 8 KiB, evidence ≤ 20 items, at least one anchor (flow: ≥ 2 steps; relation: from and to).
    - Redacts `title`, `body` and `evidence`.
    - Writes `:Knowledge {…, workspace, layer:'semantic'}` plus `ABOUT/STEP{order}/FROM/TO {anchor_key, anchor_hash}` edges. When `embed::status` is `Vector`, it then writes the entry's chunks: `chunk::split` with header `title + tags` and body `body`, as `:Chunk {id: chunk:{k_id}:{ord}, layer:'semantic'}-[:CHUNK_OF]->(k)`, and embeds them. `revise` writes chunks for the new entry; a retracted entry keeps its chunks, and search filters them out.
    - Retries up to 3 times on a Memgraph transaction conflict, matching the error text "Cannot resolve conflicting transactions" (verified at v3.14.0).
  - `semantic::get(ctx, id) -> Result<Entry>`, and `semantic::about(ctx, node_ids: &[String], include_stale: bool) -> Result<Vec<Entry>>`: entries anchored (`ABOUT`/`STEP`/`FROM`/`TO`) to any of the nodes, excluding `retracted`.
  - `semantic::revise(ctx, id, patch: Patch) -> Result<String>`: a new entry that `SUPERSEDES` the old one; the old one gets `status: retracted`.
  - `semantic::retract(ctx, id, reason) -> Result<()>`: sets `status: retracted` and appends the reason. It never deletes.
  - `semantic::reattach_vanished(tx: &mut Connection, vanished: &[VanishedAnchor]) -> Result<(usize /* reattached */, usize /* orphaned */)>`: runs **inside** `store::index`'s transaction (Task 7 step d2). It's the vanished half of the rules below.
  - `semantic::reconcile(ctx, report: &IndexReport) -> Result<ReconcileReport { stale: usize, orphaned: usize, reattached: usize }>`: runs after commit and does only the hash-based stale marking for surviving anchors. Implements the layer design's §4.3:
    - **Surviving anchors:** recompute `anchor_hash`. If it differs: `status: stale`, `stale_since: run_id`.
    - **`report.vanished`:** if exactly one symbol in the same `unit` has the same `qualname`, re-create the edge to it and mark the entry stale. Otherwise add the `anchor_key` to `orphaned_anchors`; once the entry has no live anchors, set `status: orphaned`.
    - **Returning code:** for every entry with `orphaned_anchors`, each id that exists again gets its edge re-created and is removed from `orphaned_anchors`. The status becomes `current` if the stored `anchor_hash` matches, else `stale`. Branch switches and file-size-limit round trips therefore restore knowledge instead of losing it.
    - `retracted` entries are skipped.
  - `semantic::export(ctx, dir: &Path) -> Result<usize>`: writes `<dir>/knowledge.jsonl`, one entry per line, with no `:Chunk` data (import recomputes it).
    - Sorted by id. Keys within each object are sorted. Anchors are given by `anchor_key`, never by internal ids or vectors.
    - Ends with a trailing newline.
  - `semantic::import(ctx, dir: &Path) -> Result<ImportReport { created, updated, skipped }>`: merges by id, and the newer `updated_at` wins.
    - Errors if the workspace has no `IndexRun` ("run `ops index` first").
    - Then runs `reconcile` and re-embeds what it imported.
  - CLI:
    - `ops knowledge export [--root] [--out ops-knowledge/]`
    - `ops knowledge import [--root] [--from ops-knowledge/]`
    - `ops check` warns when the newest `Knowledge.updated_at` is later than the export file's mtime.
    - `ops init` must not add `ops-knowledge/` to `.gitignore`.

- [ ] **Step 1: Failing tests**
```rust
#[test] fn create_validates() { /* body 9 KiB → Err; no anchors → Err; flow with 1 step → Err (pure validation fn `NewEntry::validate`) */ }
#[test] fn export_is_deterministic() { /* two exports of the same entries in a different insertion order are byte-identical; no "embedding" key appears */ }
#[test] #[ignore] fn knowledge_survives_reindex() { let ctx = tmp_fixture_ctx(); store::index(&ctx, IndexMode::Full, Duration::ZERO).unwrap();
  let k = semantic::create(&ctx, summary_about("pay")).unwrap(); edit(&ctx, "services/worker/worker.py", |s| s + "\n# touch\n");
  store::index(&ctx, IndexMode::Incremental, Duration::ZERO).unwrap(); store::index(&ctx, IndexMode::Full, Duration::ZERO).unwrap();
  let e = semantic::get(&ctx, &k).unwrap(); assert_eq!(e.status, Status::Current); assert_eq!(e.anchors.len(), 1); }
#[test] #[ignore] fn stale_on_body_change() { /* summary about pay; edit pay's body; incremental index → status Stale, stale_since == report.run_id; whitespace-only edit → still Current */ }
#[test] #[ignore] fn reattach_on_move_and_orphan_on_delete() { /* move `charge` to services/orders/app/billing.py → entry re-anchored to the new id, Stale; delete charge entirely → Orphaned with orphaned_anchors == [old id] */ }
#[test] #[ignore] fn orphan_reattaches_when_code_returns() { /* summary about charge; delete app/main.py → index → Orphaned; restore the file unchanged → index → Current, anchors == [charge id], orphaned_anchors empty; restore it with charge's body changed → Stale */ }
#[test] #[ignore] fn crash_during_reattach_keeps_anchors() { /* test hook makes reattach_vanished fail → the transaction rolls back: the old nodes, the ABOUT edges and the entry status are all unchanged */ }
#[test] #[ignore] fn secret_redacted_on_write() { /* body "token=abc123" → stored body contains "token=***" */ }
#[test] #[ignore] fn import_after_index() { /* export → wipe Memgraph → import before index → Err "run `ops index` first"; index then import → entries Current, anchors attached */ }
```
- [ ] **Step 2: Run.** `cargo test -p engine semantic`. Expected: FAIL.
- [ ] **Step 3: Implement.** Add `uuid` (v4) and run `cargo deny check`.
- [ ] **Step 4: Run.** Unit tests PASS. With Docker, the `--ignored` tests PASS.
- [ ] **Step 5: Commit.** `git commit -am "feat: semantic layer core, drift reconciliation, export/import"`
