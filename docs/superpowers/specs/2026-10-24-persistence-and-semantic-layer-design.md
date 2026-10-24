# Persistence, incremental indexing and the semantic layer — Design

Date: 2026-10-24 · Status: accepted · Extends: `2026-10-24-ops-support-design.md`

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
- **Search by meaning** over both the semantic layer and the code, using Memgraph's native
  embeddings, vector and full-text indexes (Community edition), with no ML dependency in ops-support.
- Everything existing tools return (`code_context`, `trace_flow`, `service_map`, `contract`)
  includes the relevant semantic entries, with stale ones flagged.

## Non-goals
- **No incident memory in the graph.** Incident reports are generated with `write_rca` and saved
  as Markdown files. They are the first reference for future incidents (see "Incident reports"
  below). They are not graph nodes.
- No LLM inside ops-support. The semantic layer is produced by the client's LLM.
- No embedding model inside ops-support itself. Embeddings, vector search and full-text search
  are native Memgraph features (§4.5); ops-support only calls them.

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
  - ./docker/memgraph/lib:/var/lib/memgraph
  - ./docker/memgraph/log:/var/log/memgraph
```
- `docker/` is already git-ignored (`/docker/` in `.gitignore`). The named volumes `memgraph_data` / `memgraph_log` go away.
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

**Forced full re-parse.** `IndexRun` stores two fingerprints:
- **`parse_fingerprint`** covers the ops version plus the config sections that affect extraction:
  `services`, `shared_libs`, `contracts`, `exclude`, `docker`, `indexing` (§3.1), and the
  *resolved* chunk parameters (§4.5.1). When it changes, every file is treated as `changed`.
- **`embed_fingerprint`** covers `model`, `dimensions`, `metric` and `scalar_kind`. When it
  changes, the vector indexes are rebuilt and everything re-embedded, but nothing is re-parsed.

### 3.1 File size limits (configurable)
Chunking a *file* for parsing is deliberately not done. The parser needs the whole file to build
a correct syntax tree; symbols that cross a chunk boundary would be lost or misread, and large
files parse fine anyway. The size limit exists to keep generated noise (bundles, lockfiles,
vendored code, generated clients) out of the graph, so it's a setting:

```yaml
indexing:
  max_file_bytes: 1MiB          # B / KiB / MiB / GiB, or a plain number of bytes; 0 = no limit
  skip_minified: true           # *.min.js / *.min.css, and files whose average line exceeds 500 chars
  overrides:                    # first matching glob (relative to the workspace root) wins
    - { path: "services/legacy/**/*.py", max_file_bytes: 8MiB }
    - { path: "**/generated/**", skip: true }
```
- **Set right before indexing:** `ops index --max-file-bytes 8MiB`, or
  `OPS_INDEXING__MAX_FILE_BYTES=8MiB` (works for `ops watch` and `reindex` too). The flag wins
  over the env var, and the env var wins over `ops.yaml`. The flag lasts one run; the next run
  without it reverts to the configured limit. Use `overrides` or the env var for a lasting change.
- `indexing` is part of `parse_fingerprint`, so raising the limit picks the previously skipped
  files up on the next run.
- **Skips are reported, not silent.** `IndexReport.skipped` lists `{path, size, reason}`, with
  reason `too_large`, `minified`, `override_skip` or `unparseable`. Excluded directories are only
  counted. `ops index` prints the 20 largest skips and the total; `--show-skipped` prints all of
  them. `ops check` shows the count.

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
| `recall(target? , query?, kinds?, include_stale?, k?)` | Entries about a node (and its 1-hop neighbourhood), and/or a **semantic + text search** over entries (§4.5). `current` by default. |
| `search_code(query, service?, kinds?, k?)` | Find code by meaning ("where do we charge the card") over code cards (§4.5). Each hit comes with its attached current knowledge. |
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
- **The code comes back** (branch switch back, file restored, size limit raised again): an
  orphaned anchor whose `anchor_key` exists again is re-attached, and the entry becomes `current`
  if the hash matches, else `stale`.
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

### 4.5 Semantic search (native Memgraph)
Memgraph provides all three pieces, so ops-support adds no ML dependency:
- **Vector indexes:** `CREATE VECTOR INDEX … WITH CONFIG {dimension, capacity, metric}` and
  `vector_search.search`. Production-ready since Memgraph 3.2.
- **Embeddings, computed inside Memgraph:** the MAGE `embeddings` module. Its local backend runs a
  SentenceTransformer model (default `all-MiniLM-L6-v2`) in the Memgraph process; its remote backend
  calls a provider through LiteLLM. `embeddings.text([...])` returns vectors for given strings.
- **Full-text indexes:** `CREATE TEXT INDEX` and `text_search.search`. Non-experimental since 3.6.

**Edition: all three are in the free Community edition.** They're absent from Memgraph's
Enterprise-only feature list. The implementing sources (`vector_index.cpp`, `text_index.cpp`,
`src/mage/python/embeddings.py`) are BSL 1.1, which is the Community licence; Enterprise code carries
an MEL header. BSL is source-available: free to use, including commercially, but you may not offer
Memgraph as a hosted database service. Self-hosting for ops-support is fine. Checked against
Memgraph's docs and repo at v3.14.0.

**What gets embedded**

| Node | Text that gets embedded | When |
|---|---|---|
| `:Knowledge` | `title + body + tags` | on `remember` / `revise` |
| Function, Class, Component, Injectable, Schema, Route | its **chunks** (§4.5.1): each one is the symbol's *card header* (kind, qualname, signature, docstring, decorators/route, log templates, file path) followed by a slice of its **body source**. A short symbol has one chunk. | on index, only for chunks whose `text_hash` changed |

- ops-support calls `embeddings.text([...])` itself and `SET`s `chunk.embedding`. It doesn't use
  `node_sentence`, which embeds every string property.
- Long `:Knowledge` bodies (up to 8 KiB) are chunked the same way: title and tags as the header,
  then slices of the body.
- Text is already redacted when it's embedded.
- The Part 1 extractors also capture docstrings / JSDoc for the card.

#### 4.5.1 Chunking long symbols (configurable)
Embedding models read a bounded number of tokens (all-MiniLM-L6-v2: 256) and silently ignore the
rest. So a long function embedded as one piece is searchable only by its first lines. Each
symbol is therefore embedded as **overlapping chunks**, and search scores a symbol by its
best-matching chunk.

**Model:**
`(:Chunk {id, ord, start_line, end_line, text, text_hash, tokens_est, embedding, workspace, layer})-[:CHUNK_OF]->(symbol | :Knowledge)`
- Code chunks have id `chunk:{sym_id}:{ord}` and `layer: "structural"`. They're produced by
  `file_graph`, so incremental indexing creates, updates and deletes them like any other node.
- Knowledge chunks have id `chunk:{k_id}:{ord}` and `layer: "semantic"`. They're rewritten on
  `remember`/`revise`, never touched by indexing, and left out of the export (recomputed on import).
- `text` is stored so a model change can re-embed without re-parsing. It's redacted before it's
  stored, like everything else.

**Splitting:**
- `strategy: syntax` (default) packs the symbol body's top-level statements greedily up to the
  token budget, overlapping each chunk with the tail of the previous one by `overlap_tokens`.
- A single statement larger than the budget falls back to line windows.
- `strategy: lines` uses line windows only.
- The budget is `max_tokens` minus the header's tokens. Tokens are estimated as `ceil(chars / 4)`;
  ops-support has no tokenizer, and the estimate is deliberately conservative.
- Past `max_chunks_per_symbol`, the remaining body is dropped and the symbol gets
  `chunks_truncated: true`. `ops index` reports how many symbols were truncated.

**Configuration** (under `semantic.embeddings.chunking`):
```yaml
semantic:
  embeddings:
    chunking:
      enabled: true              # false = one chunk per symbol: header + body cut at max_tokens
      max_tokens: auto           # auto = the model's max_sequence_length from embeddings.model_info;
                                 #        remote models report none, so remote_max_tokens applies
      remote_max_tokens: 512
      overlap_tokens: 32
      max_chunks_per_symbol: 32
      strategy: syntax           # syntax | lines
      aggregate: max             # how a symbol's chunks score: max | mean_top2
```
The *resolved* values (with `auto` turned into a number) are part of `parse_fingerprint`. The
resolved `max_tokens` is stored on `IndexRun`, and reused when model info is temporarily
unavailable, so a transient failure never forces a re-parse.
Changing them re-chunks everything on the next run. A model swap that keeps the same resolved
`max_tokens` only re-embeds.

**Indexes** (created idempotently in autocommit before each index run, like the other indexes):
- `chunk_vec ON :Chunk(embedding)`, one vector index for code and knowledge chunks, filtered by
  `CHUNK_OF` target and `layer` at query time.
  `metric: "cos"`, `dimension` from `embeddings.model_info`, `capacity` = 2 × the current count.
- Text indexes: `knowledge_text ON :Knowledge(title, body)`, `chunk_text ON :Chunk(text)`, and one
  `card_text_<label> ON :<Label>(card)` per code label. Memgraph's grammar takes exactly one label
  and an optional property list: `CREATE TEXT INDEX name ON :Label(props…)` (verified at v3.14.0).

**Querying:**
1. `embeddings.text([query])`.
2. `vector_search.search("chunk_vec", k*5, vec)`, then follow `CHUNK_OF` to the symbol or entry.
   Aggregate per parent with `aggregate` (`max`, or the mean of the top two), keeping the best
   chunk's `start_line`–`end_line`.
3. `text_search.search` over the text indexes, then merge the two rankings with reciprocal-rank
   fusion.
4. Boost hits within 2 hops of `target` / `service`.
5. Drop `retracted` hits, and flag `stale` ones.

**Configuration.** It's split by *who reads it*, because the Memgraph `embeddings` module never
takes API keys as arguments. LiteLLM, inside the Memgraph process, reads them from that process's
environment (verified in `src/mage/python/embeddings.py` at v3.14.0).

**(a) At `docker compose up`: the Memgraph container** (repo `.env`, loaded with
`env_file: {path: .env, required: false}` on the memgraph service)

| Variable | Default | Purpose |
|---|---|---|
| `EMBEDDINGS_MODEL` | `all-MiniLM-L6-v2` | The model the warmup service loads and checks (below). Keep it equal to `semantic.embeddings.model` in `ops.yaml`. `ops check` can't read the container's environment, so it prints the model `ops.yaml` resolves to for you to compare; a mismatch only means the first index downloads that model. |
| `HF_HOME` | `/var/lib/memgraph/hf-cache` (set in compose) | Model cache, on the bind mount (`docker/memgraph/lib/hf-cache`), so models download once |
| `HF_TOKEN` | unset | Hugging Face token for gated or private local models |
| `HF_HUB_OFFLINE` | `0` | `1` = never download; models must already be in the cache (air-gapped) |
| `OPENAI_API_KEY` | unset | `openai/…` models |
| `AZURE_API_KEY`, `AZURE_API_BASE`, `AZURE_API_VERSION` | unset | `azure/<deployment>` models |
| `COHERE_API_KEY` | unset | `cohere/…` models |
| `VOYAGE_API_KEY` | unset | `voyage/…` models |
| `MISTRAL_API_KEY` | unset | `mistral/…` models |
| `GEMINI_API_KEY` | unset | `gemini/…` models |
| `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_REGION_NAME` | unset | `bedrock/…` models |
| *(none)* | | `ollama/…` models need no key; set `api_base` in `ops.yaml` (e.g. `http://host.docker.internal:11434`) |

Any other LiteLLM provider variable works the same way, because the whole `.env` is passed
through. Only what you set is present; an unset key matters only if a model for that provider is
chosen.

**Ways to start it:**
```sh
docker compose up -d                                          # defaults: local all-MiniLM-L6-v2, CPU, downloads on first use
docker compose --profile embeddings up -d                     # also runs `embeddings-warmup`: downloads/loads EMBEDDINGS_MODEL
                                                              # and checks the provider key now, not at first index
docker compose -f docker-compose.yml -f docker-compose.gpu.yml up -d   # local models on NVIDIA GPUs
```
- **`embeddings-warmup`** (profile `embeddings`): a one-shot service that waits for Memgraph, then
  runs `CALL embeddings.model_info({model_name: $EMBEDDINGS_MODEL}) YIELD info RETURN info;` and
  exits non-zero (with the redacted error) if the model can't load or the key is rejected.
  *Builder: confirm `mgconsole` is in the `memgraph-mage` image; if not, use the `memgraph/mgconsole`
  image for this service.*
- **`docker-compose.gpu.yml`** switches the memgraph service to the CUDA image
  `memgraph/memgraph-mage:3.14.0-relwithdebinfo-cuda` (amd64 only) and adds
  `deploy.resources.reservations.devices: [{driver: nvidia, count: all, capabilities: [gpu]}]`.
  The default image ships a CPU-only PyTorch build, so reserving the GPU alone wouldn't use it.
  It needs the NVIDIA Container Toolkit on the host. Docker Desktop on macOS has no GPU passthrough, so local models run on CPU
  there.

**(b) Per call: `ops.yaml` `semantic.embeddings`** (ops-support passes these as the configuration
map to `embeddings.text` / `embeddings.model_info`). Every field accepts a literal or
`{ env: NAME }`, and can be overridden with `OPS_SEMANTIC__EMBEDDINGS__<FIELD>`.

```yaml
semantic:
  embeddings:
    enabled: true                  # false = text search only, nothing gets embedded
    model: all-MiniLM-L6-v2        # bare name / HF path → local SentenceTransformer inside Memgraph;
                                   # "provider/model" (openai/text-embedding-3-small, voyage/voyage-3,
                                   # cohere/embed-english-v3.0, ollama/nomic-embed-text, …) → remote via LiteLLM
    # local only
    device: null                   # null (first GPU, else CPU) | cpu | cuda | all | "cuda:1" | 1 | [0, 1]
    batch_size: 2000
    chunk_size: 48                 # per-GPU chunk on the multi-GPU path
    # remote only
    api_base: null                 # override the provider endpoint (Ollama, Azure, proxies)
    input_type: document           # what cards/entries are embedded as
    query_input_type: query        # what search queries are embedded as (matters for Cohere/Voyage)
    dimensions: null               # truncate output, if the provider supports it
    timeout: 60                    # seconds per request
    num_retries: 3
    normalize: true                # L2-normalize remote vectors, like the local path
    remote_batch_size: null        # null = 96 for Cohere, 256 otherwise
    concurrency: 4                 # parallel requests, 1–32
    # vector index (ops-support)
    metric: cos                    # cos | ip | l2sq …
    scalar_kind: f32               # f32 | f16 | bf16 … (smaller = less memory, less precision)
```
- Options the module has but ops-support sets itself (`embedding_property`,
  `excluded_properties`, `return_embeddings`) are not user-facing, because ops-support always uses
  `embeddings.text`.
- `model`, `dimensions`, `metric` and `scalar_kind` are part of `embed_fingerprint`, and the resolved chunk parameters are part of `parse_fingerprint` (§3).
  Changing any of them drops and recreates the vector indexes and re-embeds every card and entry.
- `ops check` calls `embeddings.model_info` with this config and reports which path is in use
  (local, or remote with the provider name), the dimension, and any load or auth error.

**Operational notes (from Memgraph's docs)**
- Deleting nodes doesn't free vector-index memory until the index is dropped. `ops index --full`
  drops and recreates the vector indexes. `ops check` warns when an index holds over 2× as many
  vectors as there are live nodes.
- Vector indexes are `READ_UNCOMMITTED`, and the query planner never uses them. All vector access
  goes through `vector_search.*` in ops-support's own Cypher, so `graph_query` still rejects `CALL`.
- Snapshots and the WAL store vector values. The docs don't say whether index *definitions* are
  restored on restart; the idempotent creation before each run covers that either way.
- The local path downloads the model from Hugging Face on first use (or at `up` with the
  `embeddings` profile). It's cached under `HF_HOME` on the bind mount. Air-gapped setups pre-seed
  that directory and set `HF_HUB_OFFLINE=1`, or use a remote/Ollama model.
- Set `omit_vector_index_properties_on_return`, and strip `embedding` in `store::query` too, so
  vectors never appear in tool output.
- The export (§4.4) leaves vectors out. They're recomputed on import, which keeps git diffs
  readable.
- **Fallback:** if the `embeddings` module is missing (a non-MAGE image) or the model won't load,
  `recall` and `search_code` use text search only and say so. `ops check` reports which mode is
  active.
- **Image:** pin `memgraph/memgraph-mage:3.14.0` (published for amd64 and arm64), not `latest`.

### 4.6 Client integration
- Both clients connect to the same stdio server through **one file**: `ops init` writes `.mcp.json` at
  the workspace root (`{"mcpServers": {"ops": {"type": "stdio", "command": "ops-mcp", "args": ["--mode", "operational-support"]}}}`).
  - Claude Code reads it.
  - Copilot CLI is **documented** to read project-level `.mcp.json` (from the working directory up to
    the repository root, with the `mcpServers` wrapper; `tools` defaults to all). See GitHub Docs,
    "Adding MCP servers for GitHub Copilot CLI". This is still to be confirmed by hand (Part 2 Task 14).
  - Neither client documents the server's working directory, so the entry has no `--root`. `ops-mcp`
    walks up from its working directory to `ops.yaml`, or uses `OPS_ROOT`.
  - `ops init` never edits the user-level `~/.copilot/mcp-config.json`.
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

## 6. Changes to existing docs and plans (applied)

Applied in the base spec and both plans. Two refinements came out of plan review:
- Vanished-anchor handling runs inside the index transaction.
- The fingerprint is split into parse and embed parts, so a model change re-embeds without re-parsing.

- Base spec: stable ids (§2.1), the `layer` property, `body_hash`, `IndexRun`, the semantic layer
  summary, incident files, the `docker/memgraph/` bind mount; no named volumes.
- `.gitignore`: `/docker/` is already ignored; update its comment to say it holds the Memgraph bind mount (data, logs, model cache).
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
- Part 1 extractors: capture docstrings / JSDoc. New task: code cards, embedding on index, vector and text indexes. Part 2: `search_code`, vector-backed `recall`, `ops check` embedding status.
- `docker-compose.yml`: pin the MAGE image (≥ 3.6), set `HF_HOME` on the bind mount, `env_file: .env`, and the `embeddings-warmup` service (profile `embeddings`). New `docker-compose.gpu.yml`. `.env.example` gains the variables in table (a).
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
- Semantic search is hybrid (vector + full text, rank-fused, graph-boosted), all computed by
  Memgraph. Vectors are left out of the committed export and recomputed on import.

## What's left to the builder
- Exact Cypher for subgraph replacement and relinking; batch sizes.
- Debounce details and ignore-file handling in `ops watch`.
- Fusion constant and graph-boost weights for search ranking (RRF with k=60 is a fine start).
