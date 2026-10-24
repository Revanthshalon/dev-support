# Persistence, incremental indexing and the semantic layer — Design

Date: 2026-10-24 · Status: draft for review · Extends: `2026-10-24-ops-support-design.md`

## Intent
The graph in Memgraph is worth more than what a parser can produce. The code index takes time to
build. On top of it, LLM clients (Claude Code, Copilot CLI) build up an *understanding* of the
system: what a function is for, which flows matter, how two services really talk, what the
gotchas are. That understanding is expensive to produce and impossible to re-derive from source.
So:

1. The graph must **survive restarts** of the container and the machine.
2. Code changes must **update the index without re-indexing everything**.
3. LLM-produced understanding is a **separate, first-class semantic layer**. Re-indexing and code
   edits never destroy it. When the code it describes changes, it's marked for review, not
   silently kept as if still true.

## Goals
- Memgraph data on a **local directory** (bind mount), so it's visible, backed up like any other
  directory, and kept across `docker compose down`.
- `ops index` is **incremental by default**: only new, changed and deleted files are re-parsed,
  then cross-file links are recomputed. `ops index --full` rebuilds the structural layer from scratch.
- `ops watch` keeps the index current while you work.
- A **semantic layer** written and read through MCP tools, so it works the same from **Claude Code
  and Copilot CLI** (both run stdio MCP servers). Every entry records which client and model
  wrote it, when, from what evidence, and with what confidence.
- The semantic layer is **anchored to code by stable keys** (no line numbers) and tracks drift:
  `current → stale` when the code it describes changes, `orphaned` when the code disappears.
- Everything existing tools return (`code_context`, `trace_flow`, `service_map`, `contract`)
  includes the relevant semantic entries, with stale ones flagged.

## Non-goals
- **No incident memory in the graph.** Incident reports are generated with `write_rca` and saved
  as Markdown files. They are the first reference for future incidents (see "Incident reports"
  below). They are not graph nodes.
- No LLM inside ops-support. The semantic layer is produced by the client's LLM.
- No embeddings or vector search in this iteration. Recall is by graph anchor and text match.

## Anti-goals (a failure even if it technically works)
- A re-index or code edit that deletes or rewrites semantic entries.
- Semantic entries that silently describe code that no longer exists, or has changed, as if still
  true.
- Secrets ending up in the semantic layer: entries are redacted on write like every other output.
- A client writing arbitrary Cypher. Semantic writes go only through dedicated, schema-checked,
  audited tools; `graph_query` stays read-only.

---

## 1. Persistence (bind mount)

`docker-compose.yml` mounts Memgraph's data and log directories onto the repo:
```yaml
volumes:
  - ./.data/memgraph/lib:/var/lib/memgraph
  - ./.data/memgraph/log:/var/log/memgraph
```
- `.data/` is git-ignored. The named volumes `memgraph_data` / `memgraph_log` go away.
- Memgraph must recover from its snapshot and WAL on startup. Confirm the exact flag in
  `memgraph --help` (`--data-recovery-on-startup`) and set it explicitly in `command:`. Also set a
  periodic snapshot (`--storage-snapshot-interval-sec`, e.g. 300) and snapshot-on-exit.
- **Linux hosts:** the container runs as the `memgraph` user, so the bind-mounted dirs must be
  writable by that uid. The README documents the `chown` (look up the uid with
  `docker run --rm memgraph/memgraph-mage id memgraph`). macOS/Windows Docker Desktop needs nothing.
- A bind mount is not a backup. Section 4 gives the semantic layer its own export, because only
  it can't be rebuilt.

## 2. Two layers in one graph

| | Structural layer | Semantic layer |
|---|---|---|
| Written by | `ops index` / `ops watch` | MCP clients, through semantic tools |
| Rebuildable | Yes, from source | **No.** Only from an export (§4) |
| Node marker | `layer: "structural"` | label `:Knowledge`, `layer: "semantic"` |
| Deleted by re-index | Its own changed parts | **Never** |

All structural deletes match on `layer: "structural"`. No re-index code path may ever match
`:Knowledge`.

### 2.1 Stable ids (change to the base spec)
Structural ids drop line numbers so anchors survive edits:
- `sym:{service}:{path}:{qualname}`, where `qualname` = `Owner.name` or `outer.inner`.
- When the same qualname occurs twice in one file (overloads, property getter/setter), the second
  gets a `~2` suffix, the third `~3`, in source order.

Every Function/Class/Component/Injectable/Schema node also stores `body_hash`: a hash of its
source text with whitespace collapsed and comments stripped. Other ids are unchanged; none of
them contained lines except `log:`. `log:` becomes `log:{sym_id}:{n}`, where `n` is its order
within the function.

## 3. Incremental indexing

**Per file:** every `File` node stores `hash` (content hash) and `size`. A run:
1. **Plans.** Walk the workspace (same excludes and limits as today) and compare against the
   graph's `File {path, hash}` set. This gives `added`, `changed`, `deleted` and `unchanged`.
2. **Upserts files. Nodes are never deleted and recreated.** For each `added` or `changed` file:
   - Parse it, then `MERGE` every node it produces by stable id and `SET` its properties
     (including the new `body_hash`).
   - Replace that file's own structural edges (`CONTAINS`, `DEFINES`, `HAS_METHOD`, `LOGS`,
     `READS_ENV`, `HANDLED_BY`).
   - Delete only the nodes the file **no longer produces**.

   For each `deleted` file, delete its nodes. Nodes that survive keep every `ABOUT`/`STEP` edge
   untouched; only vanished nodes involve the semantic layer (§4.3). Unresolved call names, HTTP
   calls, used types and injections are stored **on the nodes** as properties, so linking never
   re-parses unchanged files.
3. **Re-derives the small stuff.** Docker compose, Dockerfiles, config services, datastores and
   contracts are rebuilt every run. They're small, and cross-cutting.
4. **Relinks.** Delete and recompute every *link* edge (`CALLS`, `CALLS_API`, `USES_SCHEMA`,
   `IMPLEMENTED_BY`, `INJECTS`, `TEMPLATE_BINDS`, `USES_LIB`, `CONSUMES`, `BOUNDARY`) from the facts
   stored in the graph. Linking is in-memory over node properties, so it's fast compared with
   parsing.
5. **Reconciles the semantic layer** (§4.3).
6. **Records the run:** a `(:IndexRun {id, started_at, finished_at, mode: incremental|full, files_added, files_changed, files_deleted, git_head?})` node, keeping the last 20.

Steps 2–4 run in one Memgraph transaction, so a failed run leaves the previous graph intact.
Indexes are created beforehand, in autocommit (base plan, Task 7).

**`ops index --full`** re-parses every file the same upsert way, then deletes structural nodes the
run didn't produce, then runs §4.3. It never wipes the layer, so semantic anchors on surviving
nodes are never cut.

**Forced full re-parse.** `IndexRun` stores a `fingerprint`: the ops version plus a hash of the
config sections that affect extraction (`services`, `shared_libs`, `contracts`, `exclude`,
`docker`). When it differs from the last run, every file is treated as `changed`.

**One writer at a time.** `ops index`, `ops watch` and the MCP `reindex` tool all take an exclusive
lock file, `.ops/index.lock` (holding pid and start time; locks left by a dead process are taken
over). A second indexer either waits up to `--wait` seconds (default 0) or reports
`index in progress (pid N, since T)`. Semantic writes from several `ops-mcp` processes (e.g.
Claude Code and Copilot CLI at once) retry up to 3 times on a Memgraph transaction conflict.
Builder: match the conflict error text against Memgraph's docs.

**`ops watch [--root] [--debounce-ms 1500]`** watches the workspace (respecting `exclude`) and runs
an incremental index after each burst of changes, logging a one-line summary per run.

**From MCP:** the existing graph tools add `index: {last_run_at, files_changed_since: n}` to their
output when the workspace has files newer than the last run. A new `reindex()` tool runs an
incremental index on request; it's audited. *(My call: the agent sees staleness and can fix it,
rather than the server re-indexing on its own mid-investigation.)*

## 4. Semantic layer

### 4.1 Model
```
(:Knowledge {
  id,                       // "k:" + uuid
  kind,                     // see table
  title,                    // ≤ 120 chars
  body,                     // Markdown, ≤ 8 KiB, redacted on write
  confidence,               // high | medium | low
  status,                   // current | stale | orphaned | retracted
  evidence: [ ... ],        // tool calls or "file:line" strings, ≤ 20
  tags: [ ... ],
  author_client,            // from MCP initialize clientInfo, e.g. "claude-code", "copilot-cli"
  author_model,             // optional, supplied by the client
  session_id, created_at, updated_at,
  workspace,                // same isolation as the structural layer
  layer: "semantic"
})
(:Knowledge)-[:ABOUT {anchor_key, anchor_hash}]->(structural node)   // 1..n anchors
(:Knowledge {kind:"flow"})-[:STEP {order}]->(structural node)
(:Knowledge {kind:"relation"})-[:FROM]->(node), -[:TO]->(node)        // links static parsing can't see
(:Knowledge)-[:SUPERSEDES]->(:Knowledge)                              // revision history
```

| kind | What it captures | Typical anchor |
|---|---|---|
| `summary` | What this does and why it exists | Function, Class, Component, File, Service |
| `concept` | A domain or business concept and where it lives in code | Service(s), Schema(s) |
| `flow` | A named end-to-end flow ("checkout"), as ordered steps | STEP edges over Route/Function/Service |
| `relation` | A connection static parsing can't see (a queue, cron, webhook, shared table) | FROM/TO Service or Function |
| `invariant` | An assumption the code relies on but doesn't enforce | Function, Schema, Datastore |
| `gotcha` | A non-obvious behaviour or trap | any |
| `ops_note` | Operational knowledge ("drain the worker queue before restarting") | Service, Datastore, EnvVar |

### 4.2 Tools (MCP)
| Tool | Effect |
|---|---|
| `remember(kind, title, body, anchors: [id_or_name], evidence, confidence, tags?, steps?/from?/to?)` | Create an entry. Anchors resolve like `find_symbol`; ambiguous or unknown anchors are an error. Returns the id. |
| `revise(id, body?, title?, confidence?, evidence?, anchors?)` | Create a new entry that SUPERSEDES the old one (the old one becomes `retracted`). Also how a `stale` entry gets re-confirmed. |
| `retract(id, reason)` | `status: retracted`, with the reason appended. Never a hard delete. |
| `recall(target? , query?, kinds?, include_stale?)` | Entries about a node (and its 1-hop neighbourhood), or a text match over title/body/tags. `current` by default. |
| `knowledge_gaps(service?, kinds?)` | Structural nodes with no current `summary` (ranked by fan-in, route handlers and entrypoints first), plus every `stale` and `orphaned` entry. This drives enrichment. |

- All five go through `run_tool` (audit) and `redact`. `body` and `evidence` are size-checked.
- Writes go to Memgraph only through these tools; `graph_query` stays read-only.
- **Modes:** the semantic tools are part of every mode, including the MVP `operational-support`.

**Prompt `enrich_knowledge(scope)`:** a playbook for the client.
1. Call `knowledge_gaps`.
2. For each gap: `code_context`, read the code, then `remember` a `summary`, or `revise` a stale entry.
3. Record any flows, relations and gotchas it discovers.
4. Prefer evidence-backed `high` confidence entries, and never invent behaviour it didn't read.

**Prompt `diagnose`** (existing) gains a step 0: `recall` on the candidate services, plus checking
past incident reports (§5). It also ends by `remember`ing any durable `ops_note` / `gotcha` /
`relation` the investigation established, separate from the incident report.

### 4.3 Drift reconciliation (runs at the end of every index)
**Anchor hash per anchor type** (stored on the edge as `anchor_hash` when the entry is written):

| Anchor | Stale when |
|---|---|
| Function, Class, Component, Injectable, Schema | its `body_hash` changes |
| File | its content `hash` changes |
| Route | its handler function's `body_hash` changes |
| Contract | its document's content hash changes |
| Service, Datastore, EnvVar, LogStatement, Endpoint | never stale by hash; only orphaned when the node disappears |

For every `ABOUT` / `STEP` / `FROM` / `TO` edge:
- **The anchor node still exists** (same stable id) and its hash equals `anchor_hash`:
  nothing to do.
- **It exists but the hash changed:** the entry becomes `status: stale`, with
  `stale_since: <run id>`.
- **It no longer exists:** try to re-attach by `qualname` within the same service (the code moved
  file). If exactly one match, re-point the edge and mark `stale`. Otherwise mark `orphaned` and
  keep the edge's `anchor_key` so a human or agent can fix it.
- `retracted` entries are skipped.

Only nodes that vanish lose edges. Before deleting them, step 2 of §3 records
`(knowledge id, anchor_key, anchor_hash)` for their incoming semantic edges; §4.3 uses that list
for the re-attach and orphan rules above. An orphaned entry keeps `anchor_key` in
`orphaned_anchors: [...]`.

### 4.4 Export and import (the semantic layer's backup)
- `ops knowledge export [--out ops-knowledge/]` writes one JSON object per entry, edges inlined
  by `anchor_key`, sorted by id, in a deterministic format, so diffs are reviewable.
- `ops knowledge import [--from ops-knowledge/]` merges by id; newer `updated_at` wins. It runs
  **after** `ops index` (it errors when the workspace has no structural layer yet), then runs §4.3
  so anchors attach or are marked orphaned.
- `ops check` warns when the newest semantic entry is newer than the last export.
- **Decided:** `ops-knowledge/` is **committed to the workspace repo**, so the team shares the
  semantic layer and reviews changes to it in PRs. `ops init` must not git-ignore it. Export is
  deterministic, so diffs show only the entries that changed. Since it's committed, redaction on
  write (§4.2) is the guard against secrets reaching git.

### 4.5 Client integration
- Both clients connect to the same stdio server. `ops init` writes:
  - `.mcp.json` for Claude Code (as today).
  - The Copilot CLI MCP config. *Builder: confirm Copilot CLI's config location and format
    (user-level `~/.copilot/mcp-config.json`, and whether a workspace-level file is supported)
    against current Copilot CLI docs. If only user-level exists, `ops init` prints the snippet
    instead of editing the user's home directory.*
- `author_client` comes from MCP `initialize.clientInfo.name`, normalized (`claude-code`,
  `copilot-cli`, else the raw name). `author_model` is an optional tool argument.
- Optional: `ops init --agent-instructions` appends a short block to `AGENTS.md` / `CLAUDE.md`
  telling the agent to `recall` before changing or diagnosing a service, and to `remember` what it
  verified. *(My call: opt-in, because those files belong to the user.)*

## 5. Incident reports (files, not graph)
- New config `incidents: { dir: docs/incidents }`.
- `save_incident_report(markdown)` writes `<dir>/<incident-id>-<slug>.md`. The id and title are
  parsed from the template header. It never overwrites; an existing id is an error, so revisions
  get a new id or are edited by a person. *(My call: a write tool limited to one directory, so
  clients without file access, like a chat-only setup, can still store the report.)*
- `find_incident_reports(query?, service?, category?)` scans `<dir>`, reading each report's header
  table and §1/§5.1, and returns matches with path, id, status, severity, services and root-cause
  category.
- `diagnose` step 0 calls it, so past reports are the first reference.

## 6. Changes to existing docs and plans (after approval)
- Base spec: stable ids (§2.1), the `layer` property, `body_hash`, `IndexRun`, the semantic layer
  summary, incident files, the `.data/` bind mount; no named volumes.
- `.gitignore`: add `.data/`.
- Part 1 plan:
  - Task 0 is no longer "verify only": the compose file changes to the bind mount with recovery flags.
  - Task 6: stable ids, `body_hash`, facts stored on nodes.
  - Task 7: becomes "full + incremental store with relink", plus a new Task 7b `ops watch`.
  - New tasks: drift reconciliation, knowledge export/import.
- Part 2 plan:
  - New task: semantic tools and `enrich_knowledge`.
  - `diagnose` step 0.
  - `save_incident_report` / `find_incident_reports`.
  - Staleness info on graph tools, and `reindex`.
- New dependencies: a hash crate (`sha2`, already in the tree through sqlx), a file watcher
  (`notify`), and `uuid`, each passing `cargo deny`.

## Calls I made (check these while reading)
- `reindex` is an MCP tool the agent calls; the server never re-indexes on its own.
- `ops watch` is included. You asked for incremental updates, not a watcher specifically; it's a
  thin loop over incremental `ops index`.
- `save_incident_report` is a write tool, limited to the incidents directory and never
  overwriting.
- Agent instructions in `AGENTS.md`/`CLAUDE.md` are opt-in (`ops init --agent-instructions`).
- The semantic tools are in every mode, including the MVP.
- No embeddings or vector search yet.

## What's left to the builder
- Exact Cypher for subgraph replacement and relinking; batch sizes.
- Debounce details and ignore-file handling in `ops watch`.
- The text-match strategy for `recall` (case-insensitive substring over title/body/tags is enough
  to start).
