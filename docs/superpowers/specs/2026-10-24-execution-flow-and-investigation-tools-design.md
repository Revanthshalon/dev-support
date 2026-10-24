# Execution-flow analysis and investigation tools — Design

Date: 2026-10-24 · Status: draft for review · Extends: `2026-10-24-ops-support-design.md` (base spec)
and `2026-10-24-persistence-and-semantic-layer-design.md` (layer design)

## Intent
`trace_flow` says which function calls which. An investigation needs more:
- how an error travels and what the caller finally sees;
- what a specific request actually did across services;
- the async hops through queues that static calls can't see;
- which code touches the table that looks wrong;
- what changed recently;
- who is affected when something is down;
- what was rolled out and when.

This document adds those seven capabilities to the operational-support mode.

## Goals
1. **Error paths (static).** From a raise site to its outcome: caught and handled, logged,
   swallowed, re-raised as something else, or escaping to an HTTP status / failed job.
2. **Request timeline (runtime).** One cross-service timeline of what a request actually did,
   built from logs by correlation id or time window, with each line mapped to code.
3. **Async hops.** Producers and consumers of Redis lists, streams and pub/sub channels, and of
   Celery and RQ tasks, become graph edges that `trace_flow`, `error_paths` and `BOUNDARY` follow.
4. **Database access in code.** Which functions read or write which table, from SQL string literals
   and SQLAlchemy models.
5. **Recent changes.** Git history mapped onto the functions, routes and config files of the graph.
6. **Blast radius.** `service_map` can focus on a node and return what depends on it, or what it
   depends on.
7. **Rollout history.** `k8s_status` and `compose_status` report images, revisions and start times.

## Non-goals
- No live tracing or instrumentation of the running system (no OpenTelemetry collector, no
  agents). Runtime flow comes only from the logs ops-support can already read.
- No interprocedural data-flow or taint analysis. Error paths follow exception *types* along the
  call graph; they don't track values.
- No metrics source (Prometheus etc.) and no active HTTP probing in this iteration.
- No git writes of any kind.

## Anti-goals
- An error path presented as certain when it rests on a heuristic. Every hop says how it was
  determined (`exact`, `by_name`, `catch_all`, `unknown`).
- A timeline that silently reorders events across services. Cross-host clock skew is real, so
  ordering is by timestamp, and cross-service gaps under the configured skew are flagged
  `order_uncertain`.
- Tools that leak secrets through commit messages, diffs or log lines. All output is redacted;
  diffs are never returned, only line ranges and function names.

## Scope changes to the base spec
- **Static analysis scope** grows from "functions, calls, logs, routes" to also cover:
  - exceptions (raise sites, try/except handlers, FastAPI exception handlers, class bases);
  - queue operations;
  - table access.

  Python gets all of these. TypeScript gets `throw` / `try…catch` / RxJS `catchError` only, since
  the Angular frontend has no queues or SQL.
- **Worker framework:** Celery and RQ are recognized inside `framework: worker` services, by their
  decorators and calls. There's no new framework value.
- **New runtime dependency:** the `git` CLI, read-only and optional. `recent_changes` reports
  "git unavailable" when it's missing, and `ops check` reports it.
- **New config:** `correlation` (patterns for request ids) and `changes` (default look-back),
  see §8.
- **Tool count:** three new tools (`error_paths`, `request_timeline`, `recent_changes`), so 28
  becomes 31. `service_map`, `k8s_status`, `compose_status` and `db_schema` get new options or
  fields.
- **Graph:** new labels `Queue` and `Table`. New rels `SENDS_TO`, `RECEIVES_FROM`, `READS_TABLE`,
  `WRITES_TABLE` and `MAPS_TO`. `CALLS` edges gain a `lines` property, and `BOUNDARY.via` gains
  `queue` and `table`.

---

## 1. Error paths

### 1.1 What the parser records (per function or method)
| Fact | Python source | TypeScript source |
|---|---|---|
| `calls[] {name, line}` | every call (already recorded; now **with line**) | same |
| `raises[] {type, line, status?}` | `raise X(...)` / `raise X`. `status` comes from `HTTPException(status_code=N)` or `status.HTTP_404_…` | `throw new X(...)` |
| `handlers[] {types[], try_start, try_end, line, action, status?, reraise_as?}` | each `except` clause of a `try_statement`. `types` are its exception names (an `as_pattern` wrapping a `tuple` holds several; bare `except` gives `[]`, meaning all). `action` is `reraise` if its block raises (`reraise_as` = the new type), `return` if it returns, `log` if it only logs, `swallow` if it only passes or continues | `catch_clause` (types `[]`), and `.pipe(catchError(...))` around a call, which counts as a handler covering that call's line |
| `bases[]` | `class X(A, B)`, for exception classes | `class X extends A` |
| `handles_exception {type, status?}` | `@app.exception_handler(X)`, where `status` is a literal `status_code=` in a returned response | — |

### 1.2 How a path is computed (`flow::error_paths`)
1. **Start.** Begin from the raise sites of the target. If the target is a route, log statement or
   function, include its own raises and those of its callees up to `depth` (default 4, max 8).
2. **Climb the callers.** For each raise of type T, walk up `CALLS` edges. At each caller, take
   the `lines` of the call to the callee, and find a handler whose `try_start..try_end` covers one
   of those lines and whose types match T.
3. **Matching** compares names. A handler type matches when it equals T, or is an ancestor of T
   through `bases` resolved within the workspace graph (`exact`). It also matches when it's
   `Exception` / `BaseException` / a bare `except` / a JS `catch` (`catch_all`). Exception classes
   that aren't defined in the workspace match by name only (`by_name`).
4. **Outcome at a matching handler:** `handled` (return), `logged` (log only), `swallowed` (pass),
   or `reraised_as X`. A re-raise continues the walk as a new raise of type X, with `status` taken
   from an `HTTPException`.
5. **Escaping the top:**
   - At a **route handler**: a `handles_exception` mapping for T (or an ancestor), **registered in
     the same service as the route**, gives its status. Each FastAPI app has its own handlers. Otherwise an `HTTPException` gives its own status, and FastAPI's default is
     `500 Internal Server Error`.
   - At a **worker entrypoint** or **queue consumer**: `job fails`, plus `retried` when the Celery
     task has `autoretry_for` covering T, or `bind=True` with `self.retry`.
   - At an **Angular component method**: `uncaught in UI`.
6. **Recursion and limits.** Recursion stops at a visited node. A call whose callee didn't resolve
   ends that branch with `unknown`.

`// ponytail:` name-based types without a full class hierarchy for third-party exceptions;
upgrade with LSP type resolution if `by_name` hops turn out wrong in practice.

### 1.3 Tool
`error_paths(target: String, exception: Option<String>, depth: Option<u8>)` returns:
```json
{ "paths": [ { "raise": {"type": "PaymentGatewayError", "at": "services/orders/app/main.py:41", "in": "charge"},
               "hops": [ {"function": "pay", "at": "services/orders/app/main.py:30", "outcome": "reraised_as HTTPException", "status": 502, "match": "catch_all"} ],
               "result": {"kind": "http_status", "status": 502, "route": "POST /api/orders/{order_id}/pay"} } ],
  "unresolved": 0 }
```
Each path is capped at 20 hops, and the tool returns at most 50 paths, sorted with escaping
(unhandled) paths first.

## 2. Request timeline

### 2.1 Correlation ids (config)
```yaml
correlation:
  patterns:                   # each regex must have a named group `id`; first match per line wins
    - '(?i)\b(?:request|trace|correlation)[_-]?id["'']?\s*[:=]\s*["'']?(?P<id>[A-Za-z0-9-]{8,64})'
    - '(?i)x-request-id["'']?\s*[:=]\s*["'']?(?P<id>[A-Za-z0-9-]{8,64})'
    - 'traceparent["'']?\s*[:=]\s*["'']?00-(?P<id>[0-9a-f]{32})-'
  clock_skew_ms: 500          # cross-service gaps smaller than this are flagged order_uncertain
```
Those three patterns are the defaults. `ops init` writes them, commented, and `ops check`
compiles them and reports any that lack an `id` group.

### 2.2 Tool
`request_timeline(id: Option<String>, services: Option<Vec<String>>, since: Option<String>, around: Option<String> /* RFC 3339 */, window: Option<String> /* "2m", max "30m" */)`:
1. **Choose services:** the named services, or every service with a log source.
2. **Fetch logs** with `logs::fetch`, using the existing sources, caps and redaction. With `id`, the
   grep is the literal id, and lines are kept only when a correlation pattern extracts exactly that
   id. Without `id`, `around` ± `window/2` is required.
3. **Parse timestamps.** Recognized forms: compose `--timestamps` (`svc-1  | 2026-…Z msg`),
   kubectl `--timestamps --prefix` (`[pod/x/c] 2026-…Z msg`), and lines starting with RFC 3339 or
   `YYYY-MM-DD HH:MM:SS[,.]mmm`. Lines with no timestamp attach to the previous event; this is how
   traceback blocks stay together.
4. **Merge** events from all services and sort them by timestamp.
5. **Annotate** each event with `{ts, service, level, line, match}` (via `logmatch`). Between
   consecutive events of different services, add `hop {from, to, gap_ms, via}`, where `via` is
   the `BOUNDARY.via` between those services from the graph, or `none` for no known link.
   A `gap_ms` below `clock_skew_ms` gets `order_uncertain`.
6. **Return** the events (max 500), hops, `services_without_events`, and an `unmatched` count.

## 3. Async hops (queues)

### 3.1 Parser (Python)
| Pattern | Op | Queue name | `flavor` |
|---|---|---|---|
| `<redis>.lpush/rpush/lpushx/rpushx(key, …)` | send | literal `key` | `redis_list` |
| `<redis>.brpop/blpop/lpop/rpop/brpoplpush/lmove/blmove(key, …)` | receive | `key` | `redis_list` |
| `<redis>.xadd(key, …)` | send | `key` | `redis_stream` |
| `<redis>.xread/xreadgroup(streams={key: …})` / `(…, key, …)` | receive | `key` | `redis_stream` |
| `<redis>.publish(channel, …)` | send | `channel` | `pubsub` |
| `<pubsub>.subscribe/psubscribe(channel)` | receive | `channel` | `pubsub` |
| `@<app>.task(name="x")` / `@shared_task` on `def f` | receive | `x`, else the module path + `.f` | `celery` |
| `f.delay(…)` / `f.apply_async(…)` / `<app>.send_task("x")` | send | recorded as `task_ref` (the receiver `f`, or the literal `x`), then resolved by the link step against a **workspace-wide** index of task names and task function names. Celery task names are global, so service-scoped call resolution would miss tasks in other services | `celery` |
| `Queue("q").enqueue(f, …)` / `<q>.enqueue(…)` | send | `q` (`default` if unnamed) | `rq` |
| an RQ worker started with `Worker([...queues])` | receive | each listed name | `rq` |

- `<redis>` is any receiver: the method name and a literal first argument decide it.
- f-string keys keep their literal prefix plus `{}` (e.g. `jobs:{}`), and match by that pattern.
- Keys that aren't literals are skipped and counted in `Stats.unresolved_queue_ops`.

### 3.2 Graph
- `(:Queue {id: "queue:{flavor}:{name}", name, flavor, layer: "structural"})`
- `(:Function)-[:SENDS_TO {lines}]->(:Queue)` and `(:Function)-[:RECEIVES_FROM {lines}]->(:Queue)`
- `(:Queue)-[:ON]->(:Datastore)` when the producing and consuming services use exactly one Redis
  datastore.
- Queue nodes are **link-step nodes** (§7): derived across files and recomputed on every relink.
- `trace_flow` follows `SENDS_TO → RECEIVES_FROM`, so "down" from the API reaches the worker.
  `error_paths` treats a consumer as a job boundary (§1.2 step 5).
- `BOUNDARY.via` adds `queue` for producer service → consumer service.

## 4. Database access

### 4.1 Parser (Python)
- **SQL literals:** a string argument whose text starts (after whitespace) with `SELECT`,
  `INSERT`, `UPDATE`, `DELETE` or `WITH`.
  - Tables come from `FROM`, `JOIN`, `INTO`, `UPDATE` and `DELETE FROM`, followed by
    `[schema.]name` (with quotes stripped).
  - `INSERT`/`UPDATE`/`DELETE` targets are writes, everything else is reads, and `via` is `sql`.
  - `// ponytail:` a regex over literals, not a SQL parser. Switch to `sqlparser` if CTE-heavy SQL
    is mis-attributed.
- **SQLAlchemy:** `__tablename__ = "x"` on a class gives `Symbol.table = "x"`.
  - In a function, `select(M)`, `session.query(M)`, `session.get(M, …)` and `M.query` read M's
    table.
  - `session.add(M(...))`, `session.delete(…)`, `session.merge(…)`, `update(M)`, `insert(M)` and
    `delete(M)` write it. Here `via` is `orm`.
  - The parser records only the **model name** (`DbOp.model`). The link step resolves the model to
    its class's `table` using the usual order (same file, same service, used libraries), because
    models usually live in a shared library.

### 4.2 Graph
- `(:Table {id: "table:{datastore}:{schema.}name", name, schema, layer: "structural"})`.
  - `datastore` is the Postgres datastore of the **service doing the access** (its single one,
    else `_`).
  - A model defined in a library takes the datastore of each service that references it, and its
    `MAPS_TO` points at each of those table nodes. `_` is used only when no service references it.
  - This keeps one table one node across services and shared libraries, so `BOUNDARY via table`
    sees shared-database coupling.
- `(:Function)-[:READS_TABLE {via, lines}]->(:Table)`, `(:Function)-[:WRITES_TABLE {via, lines}]->(:Table)`
- `(:Class)-[:MAPS_TO]->(:Table)`
- Table nodes are link-step nodes (§7).
- `BOUNDARY.via` adds `table` when two services both access one table: shared-database coupling.
- `db_schema(datastore, table)` adds `used_by: [{function, service, access, via, at}]`.

## 5. Recent changes

`recent_changes(service: Option<String>, since: Option<String> /* default changes.since: "7d", max "90d" */, path: Option<String>)`:
1. **Find the git root** of each selected service path with `git -C <path> rev-parse --show-toplevel`.
   Multi-repo workspaces have several. If git is missing or there's no repository: `{"git": "unavailable"}`.
2. **List commits:** `git -C <root> log --since=<since> --no-merges --format=%H%x1f%an%x1f%aI%x1f%s --name-only -- <service path>`,
   capped at 100 commits.
3. **Map each changed file to its hunks** with `git -C <root> show --format= -U0 <sha> -- <file>`.
   Parse only the `@@ -a,b +c,d @@` headers, never the diff text. Then map the new-side ranges onto
   Function/Class/Route nodes by `start_line..end_line` from the **current** index.
   `// ponytail: maps against HEAD line ranges; older commits on heavily edited files map approximately — flagged "approximate": true when the file changed again later.`
4. **Flag config changes:** compose files, Dockerfiles, `ops.yaml`, `.env.example`,
   `requirements*.txt`, `pyproject.toml`, `package.json`, OpenAPI documents, and migration
   directories (`migrations/`, `alembic/`).
5. **Uncommitted changes:** `git status --porcelain -- <path>` is listed as `working_tree`.

It returns commits `{sha, author, date, subject (redacted), files[], functions[], routes[], config_changes[], approximate}`,
newest first. Arguments are validated: `since` against `^\d+[dhw]$`, and `path` must fall under a
configured path. Every command runs as argv with `GIT_TERMINAL_PROMPT=0`.

## 6. Blast radius and rollout history

**`service_map(focus?, direction?, depth?)`:**
- With no `focus`, it returns the whole map as today.
- With `focus` (a service, datastore, queue or table id or name), it returns the subgraph reachable
  from that node:
  - `direction: "dependents"` (default) means "what breaks if this is down";
  - `"dependencies"` means "what this needs";
  - `"both"` returns both;
  - `depth` defaults to 3, max 6.
- It follows `BOUNDARY` (consumer → provider), `USES_DATASTORE`, `SENDS_TO`/`RECEIVES_FROM` through
  queues, and `READS_TABLE`/`WRITES_TABLE` through tables.
- Each node comes with its shortest path and the `via` of each hop.

**`k8s_status`** adds `rollouts` per deployment matching the service selector:
- `kubectl get deploy -n NS -l SEL -o json` gives `{name, revision (annotation deployment.kubernetes.io/revision), images[], updated_replicas, available_replicas, conditions}`;
- `kubectl get rs -n NS -l SEL -o json` gives the last 5 ReplicaSets `{revision, images[], created_at, replicas}`, newest first. This is the image timeline.

**`compose_status`** adds `image`, `image_id` and `started_at` per container, from
`docker compose ps --format json` plus `docker inspect --format '{{json .State.StartedAt}} {{.Image}}' <id>`.

## 7. Graph and pipeline changes (for the plans)
- **The link step also produces nodes.** `graph::link` returns `(Vec<Node>, Vec<Edge>, …)`: the
  Queue and Table nodes, plus `SENDS_TO RECEIVES_FROM READS_TABLE WRITES_TABLE MAPS_TO ON` edges.
  - Relink MERGEs the link nodes and deletes the ones no longer produced. Semantic anchors on them
    go through the vanished-anchor rules of the layer design.
  - `LINK_RELS` gains those rels.
- **CALLS edges** carry `lines: [..]` (the call sites), used by `error_paths`.
- **Symbol facts** stored on nodes (for link and flow, without re-parsing): `calls` (with lines),
  `raises`, `handlers`, `bases`, `handles_exception`, `queue_ops`, `db_ops`, `table`.
- **The fixture** gains:
  - `PaymentGatewayError(Exception)` raised in `charge`;
  - `pay` catching `Exception`, logging, and raising `HTTPException(status_code=502)`;
  - an app-level `@app.exception_handler(ValueError)` returning 422;
  - `orders` doing `r.lpush("jobs:pay", id)`, with `worker.run` doing `r.brpop("jobs:pay")`;
  - a Celery task `orders.refund` in `worker` called with `.delay` from `orders`;
  - `INSERT INTO payments` in `pay`, plus an `Order` model with `__tablename__ = "orders"` read by `charge`;
  - a small git history created by the test setup (two commits touching `charge`, then `docker-compose.yml`).

## 8. Configuration additions
```yaml
correlation:                   # §2.1; defaults shown there
  patterns: [ ... ]
  clock_skew_ms: 500
changes:
  since: 7d                    # default look-back for recent_changes
```

## 9. Prompt changes
- **`diagnose`:**
  - after step 1, `recent_changes` on the candidate services;
  - in the logs step, `request_timeline` when a request or trace id is known;
  - in the code step, `error_paths` from the matched log statement or route, to explain the
    observed status;
  - `service_map(focus=…, direction="dependents")` for impact.
- **`explain_flow`** uses `trace_flow` (now through queues) and `error_paths`.
- **`triage_logs`** suggests `request_timeline` for the noisiest correlation ids.
- **`investigate_datastore`** uses `db_schema.used_by` and `recent_changes` (migrations).
- **The incident report template:** §5.3 "Causal chain" asks for the `error_paths` result, §3
  "Timeline" can cite `request_timeline`, and §3 / §5 cite `recent_changes`.

## Calls I made (check these while reading)
- Error paths are static and name-based, with an honest `match` label per hop, rather than
  resolved through an LSP type check (a possible upgrade).
- The request timeline uses only existing log sources and redaction. Correlation patterns are
  configurable, with W3C `traceparent` among the defaults.
- `recent_changes` returns line ranges and names, never diff text.
- Blast radius and rollout history extend existing tools instead of adding new ones.
- TypeScript gets error paths only, with no queue or SQL detection.

## What's left to the builder
- Exact receiver heuristics for Redis clients (`redis.Redis`, `aioredis`, connection pools).
- SQL regex edge cases (quoted identifiers, `ONLY`, CTE names, which aren't tables).
- Timestamp parser coverage beyond the listed formats.
