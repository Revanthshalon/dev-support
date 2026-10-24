# ops-support Part 1: Config, Init and Indexer — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `ops init` writes `ops.yaml`, `.env.example` and `.mcp.json` for a workspace; `ops index` parses Python, Angular (TS/HTML/CSS), Dockerfiles and compose files into a graph and writes it to Memgraph; `ops check` reports missing credentials and unreachable backends.

**Architecture:** All logic lives in the `engine` library. `cli` is a thin clap binary. Graph building is a pure function (`graph::build`) that returns nodes and edges, so it is tested without a database. `store` is the only module that talks to Memgraph (rsmgclient, synchronous, wrapped in `spawn_blocking`).

**Tech Stack:** Rust 2024, clap 4.6 (derive), config 0.15 (yaml), serde, serde_yaml_ng 0.10, serde_json, regex, walkdir, shlex, tree-sitter 0.27 + tree-sitter-python 0.25, tree-sitter-typescript 0.23, tree-sitter-html 0.23, tree-sitter-css 0.25, rsmgclient 4.0, tokio, anyhow.

**Spec:** `docs/superpowers/specs/2026-10-24-ops-support-design.md`

## Global Constraints
- The workspace stays `services/{engine,cli,mcp}`. The `cli` binary is named `ops`, and the `mcp` binary is named `ops-mcp`.
- Shared crate versions go in root `[workspace.dependencies]`; member crates use `{ workspace = true }`.
- No `neo4rs`. No grammars beyond python/typescript/html/css. No MySQL or SQLite drivers.
- A secret field is `Secret = Literal(String) | { env: String }`. Secret values never appear in `Debug` output, logs or errors.
- Node ids are deterministic and unique within a workspace (see Task 6).
- Every file skipped on purpose (excluded dir, >1 MiB, `*.min.js`, unparseable) is counted, not fatal.
- Mark each deliberate shortcut with a `// ponytail:` comment naming its limit and upgrade path.
- Tests that need Docker/Memgraph are `#[ignore]` and run with `cargo test -- --ignored`.
- Every task that adds or changes a dependency ends with `cargo deny check` passing. If a license isn't on the allow-list, stop and ask; don't widen `deny.toml` yourself.
- Each task ends with `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --check` clean.

## Review Focus
1. **Re-running `ops init`** on a workspace whose `ops.yaml` the user edited must not overwrite it without `--force`. Test: `init_refuses_overwrite` (Task 3).
2. **`node_modules`, `.venv`, minified bundles, huge generated files** must not be indexed and must not blow up memory. Test: `build_skips_excluded_and_large` (Task 6).
3. **Duplicate node ids** from two same-named functions in one file (nested `def`, overloads, `@property` getter and setter) would silently merge nodes. Test: `ids_are_unique` (Task 6).
4. **Missing env var for a secret**: `ops check` and `Secret::resolve` must name the variable and never print a value. Test: `secret_missing_env_names_var` (Task 1).
5. **Re-indexing is idempotent**: running `ops index` twice gives the same node and edge counts. Test: `index_twice_same_counts` (Task 7, ignored, needs Memgraph).

---

### Task 0: Project setup (delivered by the setup commit; verify only)

Already in the repo when this plan starts. Don't redo it; only check it.

- Root `Cargo.toml`: members `services/{engine,cli,mcp}`; `[workspace.package]` (edition 2024, rust-version, `publish = false`); every shared crate version in `[workspace.dependencies]`; workspace lints (`unsafe_code = "forbid"`, clippy `all` warn).
- Binaries: `cli` → `ops`, `mcp` → `ops-mcp`. `engine` is a library stub.
- `deny.toml`: advisories (vulnerable/unmaintained/yanked = deny), license allow-list, bans (wildcards deny, multiple-versions warn), sources (crates.io only).
- `docker-compose.yml`: memgraph uses named volumes `memgraph_data` and `memgraph_log`; credentials come from `.env`.
- `.env.example`, `.gitignore`, `rust-toolchain.toml`, and a `README.md` covering overview, architecture, configuration and setup.
- CI: `.github/workflows/ci.yml` runs fmt, clippy, test and `cargo deny check`.

- [ ] **Step 1: Verify.** `cargo build --workspace && cargo deny check && docker compose config -q`. Expected: all exit 0.
- [ ] **Step 2: Branch** for Part 1: `git checkout -b feat/indexer`.

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
#[test] fn parses_spec_example() { let c: Config = serde_yaml_ng::from_str(SPEC_YAML).unwrap();
  assert_eq!(c.services[0].framework, Framework::Fastapi);
  assert!(matches!(c.datastores[1].url, Secret::Literal(_)));
  assert_eq!(c.kubernetes.unwrap().namespaces, vec!["shop"]); }
#[test] fn secret_missing_env_names_var() { let err = Secret::Env{env:"NOPE_X".into()}.resolve(&ctx_tmp()).unwrap_err().to_string();
  assert!(err.contains("NOPE_X")); }
#[test] fn secret_debug_hides_literal() { assert!(!format!("{:?}", Secret::Literal("hunter2".into())).contains("hunter2")); }
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
- Create: `services/engine/src/docker.rs`

**Interfaces:**
- Produces:
  - `docker::ComposeService { name, image: Option<String>, build: Option<String> /* workspace-relative context */, dockerfile: Option<String>, depends_on: Vec<String>, environment: Vec<(String,String)>, env_files: Vec<String>, ports: Vec<String>, volumes: Vec<String> /* named volumes only */, file: String }`
  - `docker::parse_compose(src: &str, base: &Path, file: &str) -> Result<Vec<ComposeService>>`
  - `docker::load_compose(ctx: &Ctx) -> Result<Vec<ComposeService>>`: every `docker.compose_files` entry, with `env_file` contents merged into `environment` (file values lose to inline ones).
  - `docker::connections(svc, all) -> Vec<(String /*target*/, String /*env key*/)>`
  - `docker::Dockerfile { base_image: Option<String>, expose: Vec<u16>, env: Vec<(String,String)>, entrypoint: Vec<String> /* ENTRYPOINT ++ CMD, exec or shell form */ }`
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
#[test] fn this_repos_compose_parses() { /* parse ../../docker-compose.yml: memgraph has volumes ["memgraph_data","memgraph_log"], lab depends_on ["memgraph"] */ }
```
- [ ] **Step 2: Run.** `cargo test -p engine docker`. Expected: FAIL.
- [ ] **Step 3: Implement.**
  - Compose goes through `serde_yaml_ng::Value`; the schema is too loose for derive.
  - Sort `connections` output so tests are deterministic. The hostname regex is `(^|[/@=\s,])NAME(:\d+|/|$|\s|,)`.
  - Dockerfile: join `\` continuation lines. `ENV k=v k2=v2` and the legacy `ENV k v` forms. Shell-form CMD is split with `shlex`. A later CMD/ENTRYPOINT replaces an earlier one, and the last stage wins.
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
  - `discover::env_names(cfg: &Config) -> Vec<String>` (sorted, deduped)

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
#[test] fn written_config_round_trips() { /* Ctx::load(tmp) after write_files succeeds; env_names contains "MEMGRAPH_USER","MEMGRAPH_PASSWORD","DB_DATABASE_URL","CACHE_URL" */ }
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
  - **`.mcp.json`:** `{"mcpServers":{"ops":{"command":"ops-mcp","args":["--root","."]}}}`.
  - **`.gitignore`:** append `.env` if a `.gitignore` exists and lacks it.
  - Sort services and datastores by name.
- [ ] **Step 4: Add the CLI subcommand.** `ops init [--root DIR] [--force]` prints each written path, then: "edit ops.yaml, copy .env.example to .env, run `ops check`".
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
  - `Symbol { kind: SymbolKind, name: String, owner: Option<String>, start_line: usize, end_line: usize, signature: String, decorators: Vec<String>, calls: Vec<String>, logs: Vec<LogStmt>, routes: Vec<Route>, http_calls: Vec<HttpCall>, env_reads: Vec<String>, injects: Vec<String>, entrypoint: bool, selector: Option<String>, template_url: Option<String> }`
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
  assert!(pay.calls.contains(&"charge".into()) && pay.calls.contains(&"get_db".into())); // Depends(get_db) counts as a call
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
#[test] fn nested_and_duplicate_defs() { // def a(): def inner(): ...; and two `def prop` under @property/@prop.setter → 3 symbols, distinct start_line
}
```
- [ ] **Step 2: Run.** `cargo test -p engine parse::python`. Expected: FAIL.
- [ ] **Step 3: Implement `python::extract(src) -> FileFacts`** with a recursive walk over the tree-sitter tree.
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
#[test] fn ts_env_reads() { /* environment.apiUrl → env_reads ["environment.apiUrl"]; process.env.FOO → ["FOO"] */ }
```
- [ ] **Step 2: Run.** `cargo test -p engine parse::`. Expected: FAIL for the new tests.
- [ ] **Step 3: Implement.**
  - **Grammars:** `tree_sitter_typescript::LANGUAGE_TYPESCRIPT` for both `.ts` and `.js`, `tree_sitter_html::LANGUAGE`, `tree_sitter_css::LANGUAGE`.
  - **TS symbols:**
    - `class_declaration`: a decorator `@Component({...})` makes it a `Component`, reading `selector` and `templateUrl` from the object literal. `@Injectable` makes it an `Injectable`.
    - `method_definition` / `function_declaration` / an arrow function assigned to a `const` make Functions or Methods.
    - Constructor parameters with a type annotation, plus `inject(X)`, fill `injects`.
  - **TS HTTP calls:** `this.http.<verb>(arg)` or `<anything named http|httpClient>.<verb>`, where `arg` is a template string or string literal. Run the path through `normalize_path`.
  - **TS logs:** `console.error|warn|info|log|debug` (`log` → `info`) and `this.logger.<level>`.
  - **HTML:** attributes named `(event)` whose value starts with an identifier followed by `(` give a binding. Tag names containing `-` give `elements`.
  - **CSS:** `rule_set` selectors text, whitespace-collapsed.
- [ ] **Step 4: Run.** Expected: PASS.
- [ ] **Step 5: Commit.** `git commit -am "feat(engine): angular ts/html/css extractor"`

---

### Task 5b: OpenAPI contracts

**Files:**
- Create: `services/engine/src/contract.rs`

**Interfaces:**
- Produces:
  - `contract::OpenApi { title: Option<String>, endpoints: Vec<(String /*METHOD*/, String /*path*/)>, schemas: Vec<String> }`
  - `contract::parse_openapi(src: &str) -> Result<OpenApi>`: YAML or JSON (try JSON first, then YAML), OpenAPI 3.x. Endpoints come from `paths.*.{get,post,put,patch,delete,head,options}`, with `servers[0].url`'s path prepended when it has one. Schemas are the keys of `components.schemas`.

- [ ] **Step 1: Failing tests**
```rust
#[test] fn parses_fixture() { let o = parse_openapi(ORDERS_OPENAPI).unwrap(); assert_eq!(o.endpoints, [("POST".into(), "/api/orders/{order_id}/pay".into())]); assert_eq!(o.schemas, ["PayRequest"]); }
#[test] fn server_base_path_and_json() { let o = parse_openapi(r#"{"openapi":"3.0.0","servers":[{"url":"https://x.io/v1"}],"paths":{"/a":{"get":{}}}}"#).unwrap(); assert_eq!(o.endpoints, [("GET".into(), "/v1/a".into())]); }
#[test] fn rejects_non_openapi() { assert!(parse_openapi("services: {}").is_err()); }
```
- [ ] **Step 2: Run.** `cargo test -p engine contract`. Expected: FAIL.
- [ ] **Step 3: Implement** with `serde_json::Value` / `serde_yaml_ng::Value`. No OpenAPI crate.
- [ ] **Step 4: Run.** Expected: PASS.
- [ ] **Step 5: Commit.** `git commit -am "feat(engine): openapi contract parsing"`

---

### Task 6: Graph builder (pure)

**Files:**
- Create: `services/engine/src/graph.rs`
- Test: `services/engine/tests/graph.rs`

**Interfaces:**
- Consumes: `Ctx`, `docker::*`, `parse::*`
- Produces:
  - `graph::Graph { nodes: Vec<Node>, edges: Vec<Edge>, stats: Stats }`
  - `Node { label: &'static str, id: String, props: serde_json::Map<String, Value> }`
  - `Edge { rel: &'static str, from: String, to: String, props: serde_json::Map<String, Value> }`
  - `Stats { files_indexed: usize, files_skipped: usize, unresolved_calls: usize }`
  - `graph::build(ctx: &Ctx) -> Result<Graph>`
  - Labels and rels are exactly those in the spec's graph model. Every node gets `props.workspace = ctx.config.workspace.name`.
  - **Node ids:**
    - `svc:{name}`, `ds:{name}`, `env:{NAME}`
    - `file:{svc}:{relpath}`
    - `sym:{svc}:{relpath}:{owner.}{name}:{start_line}`
    - `log:{sym_id}:{line}`
    - `route:{svc}:{METHOD}:{path}`
  - More node ids: `contract:{name}`, `endpoint:{contract}:{METHOD}:{path}`, `schema:{contract}:{name}`.
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
#[test] fn ids_are_unique() { let g = build(&ctx()).unwrap(); let mut ids: Vec<_> = g.nodes.iter().map(|n| &n.id).collect(); let n = ids.len(); ids.sort(); ids.dedup(); assert_eq!(ids.len(), n); }
#[test] fn build_skips_excluded_and_large() { /* add a 2 MiB .py and a foo.min.js to the tmp fixture; node_modules/x/index.js exists */
  let g = build(&ctx()).unwrap(); assert!(g.nodes.iter().all(|n| !n.id.contains("node_modules") && !n.id.contains("min.js") && !n.id.contains("big.py"))); assert!(g.stats.files_skipped >= 2); }
#[test] fn deterministic() { assert_eq!(serde_json::to_string(&build(&ctx()).unwrap().nodes).unwrap(), serde_json::to_string(&build(&ctx()).unwrap().nodes).unwrap()); }
```
(`id_of(g, label, "name@Owner")` is a test helper that finds the node with that label, name and optional owner.)
- [ ] **Step 2: Run.** `cargo test -p engine --test graph`. Expected: FAIL.
- [ ] **Step 3: Implement `build`.**
  - **Service nodes:** one per config service and shared lib, plus one per compose service not linked to config (`kind: infra`, `image`).
  - **Datastore nodes:** from config.
  - **Docker edges:** DEPENDS_ON, CONNECTS_TO, SHARES_VOLUME and SETS_ENV from compose (inline env, env_file and Dockerfile ENV, `source` prop). Compose names map to config service names.
  - **Files:** walk each service/lib path with `walkdir`, skipping `exclude` dirs, files over 1 MiB and files where `Lang::from_path` is `None`.
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

### Task 7: Memgraph store, `ops index`, `ops check`

**Files:**
- Create: `services/engine/src/store.rs`
- Modify: `services/cli/src/main.rs`

**Interfaces:**
- Consumes: `Ctx`, `graph::Graph`
- Produces (Part 2 depends on these):
  - `store::write(ctx: &Ctx, g: &Graph) -> Result<()>`: synchronous.
    - Ensures `CREATE INDEX ON :<Label>(id)` for every label used, and `:<Label>(workspace)`.
    - Then `MATCH (n {workspace:$ws}) DETACH DELETE n`.
    - Then nodes per label in batches of 500: `UNWIND $rows AS r CREATE (n:<Label>) SET n = r`.
    - Then edges per `(rel, from_label, to_label)`: `UNWIND $rows AS r MATCH (a:<From> {id:r.from}), (b:<To> {id:r.to}) CREATE (a)-[e:<REL>]->(b) SET e = r.props`.
    - Commits once at the end.
  - `store::query(ctx: &Ctx, cypher: &str, params: serde_json::Map<String, Value>) -> Result<Vec<serde_json::Map<String, Value>>>`: synchronous; one map per record, keyed by column. Node → `{"_labels": [...], ...props}`, Relationship → `{"_type": ..., ...props}`, Path → `{"nodes": [...], "rels": [...]}`.
  - `store::query_async(ctx: Ctx, cypher: String, params) -> Result<…>`: `tokio::task::spawn_blocking` around `query`. The connection is created inside the closure because rsmgclient's `Connection` is not `Send`.
  - `store::ping(ctx: &Ctx) -> Result<()>`

- [ ] **Step 1: Read the rsmgclient 4.0 API before coding.** Look in `~/.cargo/registry/src/*/rsmgclient-4.0.0/src/` and confirm:
  - `ConnectParams` fields (host, port, username, password, autocommit)
  - `Connection::connect`, `execute(query, Option<&HashMap<String, QueryParam>>)`, `fetchall`, `commit`
  - the `QueryParam` and `Value` variants

  Write the JSON ↔ `QueryParam`/`Value` converters to match what you find.
- [ ] **Step 2: Failing tests**
```rust
#[test] fn json_param_round_trip() { /* json!({"a":[1,"x",{"b":true}],"n":null}) → QueryParam::Map(...) with nested List/Map/Null (pure, no DB) */ }
#[test] #[ignore] fn index_twice_same_counts() { let ctx = fixture_ctx(); let g = graph::build(&ctx).unwrap();
  store::write(&ctx, &g).unwrap(); store::write(&ctx, &g).unwrap();
  let n = store::query(&ctx, "MATCH (n {workspace:$ws}) RETURN count(n) AS c", ws(&ctx)).unwrap()[0]["c"].as_u64().unwrap();
  assert_eq!(n as usize, g.nodes.len());
  let e = store::query(&ctx, "MATCH (a {workspace:$ws})-[r]->() RETURN count(r) AS c", ws(&ctx)).unwrap()[0]["c"].as_u64().unwrap();
  assert_eq!(e as usize, g.edges.len()); }
```
- [ ] **Step 3: Run.** `cargo test -p engine store`. Expected: FAIL (compile).
- [ ] **Step 4: Implement `store`.**
  - Credentials come from `memgraph.username`/`password` through `Secret::resolve`; when unset, connect without auth.
  - Error format is exactly `memgraph <host>:<port>: <redacted cause>`, never the password. Part 2's smoke test matches on the `memgraph ` prefix.
- [ ] **Step 5: CLI commands.**
  - `ops index [--root] [--dry-run]`: build, then either print `Stats` plus node counts by label (`--dry-run`) or `store::write` and print the same counts.
  - `ops check [--root]`:
    1. Print `config::validate` problems.
    2. Print every secret that doesn't resolve, by env name.
    3. Print `store::ping`.
    4. Print `kubectl --context <ctx> version --client=false` when kubernetes is configured.
    5. Exit 1 if anything failed.

    (Task 12 adds datastore pings to this list.)
- [ ] **Step 6: Run.** `cargo test -p engine` (PASS). `cargo run -p cli -- index --dry-run --root services/engine/tests/fixtures/shop` (prints counts). With Docker up: `docker compose up -d memgraph`, `MEMGRAPH_USER=ops MEMGRAPH_PASSWORD=ops cargo test -p engine -- --ignored` (PASS). `docker compose down && docker compose up -d memgraph`, and the node count is unchanged (persistence check). If the count is 0 after restart, recovery-on-startup is off. Find the exact flag with `docker run --rm memgraph/memgraph-mage --help | grep -i recover`, add it to the compose `command`, and re-run this check.
- [ ] **Step 7: Commit.** `git commit -am "feat: memgraph store, ops index, ops check"`
