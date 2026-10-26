# cli: `ops`

The command-line entry point to ops-support. `ops` sets a workspace up and keeps its graph in
Memgraph current: it discovers services and writes their configuration, checks that credentials
and backends are reachable, and indexes the code and infrastructure files into the graph that
`ops-mcp` queries.

> [!NOTE]
> **Status: stub.** The binary builds but has no commands yet. They're added by
> [Part 1 of the plan](../../docs/superpowers/plans/2026-10-24-ops-indexer.md) (Tasks 3, 7, 7b–7d).
> All logic lives in [`engine`](../engine/README.md); this crate only parses arguments and prints
> results.

## Commands *(planned)*

| Command | What it does |
|---|---|
| `ops init [--root DIR] [--force]` | Discovers services, shared libraries, API contracts and datastores, then writes `ops.yaml` (including `semantic`, `incidents`, `correlation` and `changes` defaults), `.env.example` and `.mcp.json`. Never overwrites an existing file without `--force`, and never git-ignores `ops-knowledge/`. |
| `ops check [--root DIR]` | Validates `ops.yaml`, names every credential or setting env var that isn't set, and checks that Memgraph, the datastores and (if configured) the kubectl context are reachable. Compiles the `correlation` patterns and reports any without an `id` group, and reports whether `git` is available (optional; `recent_changes` needs it). Also reports the last index run and files changed since, the embedding mode (vector local/remote with dimension, or text-only with the reason), vector-index bloat, and an `ops-knowledge/` export older than the newest knowledge. Exits 1 if anything fails. |
| `ops index [--root DIR] [--full] [--dry-run] [--wait SECS] [--max-file-bytes SIZE] [--show-skipped]` | **Incremental by default.** Re-parses only added, changed and deleted files, recomputes cross-file links (calls, queue hops between producers and consumers, table access), embeds changed code cards, and reconciles the semantic layer (entries on changed code become `stale`, on removed code `orphaned`). `--full` re-parses everything and rebuilds the vector indexes, but never deletes knowledge. `--dry-run` prints counts without touching Memgraph. `--wait` waits for another running indexer instead of failing with "index in progress". `--max-file-bytes` raises or lowers the size limit (`indexing.max_file_bytes`, default 1 MiB) **for this run only**; skipped files are listed with size and reason, and `--show-skipped` lists all of them. |
| `ops watch [--root DIR] [--debounce-ms 1500]` | Keeps the graph current: watches the workspace and runs an incremental index after each burst of changes. |
| `ops knowledge export [--root DIR] [--out ops-knowledge/]` | Writes the semantic layer to `ops-knowledge/knowledge.jsonl`, sorted and deterministic, without vectors. Commit it to the workspace repo so the team shares and reviews it. |
| `ops knowledge import [--root DIR] [--from ops-knowledge/]` | Merges an export back in (newer `updated_at` wins), re-anchors entries and re-embeds them. Run it after `ops index`. |

Global flags: `-v` / `-q` raise or lower log verbosity. The `OPS_LOG` env var overrides both
(e.g. `OPS_LOG=engine=debug`).

## Typical flow

```sh
docker compose up -d                       # from the repo root: Memgraph + Lab
ops init  --root ~/code/shop               # review the generated ops.yaml
cp ~/code/shop/.env.example ~/code/shop/.env && $EDITOR ~/code/shop/.env
ops check --root ~/code/shop
ops index --root ~/code/shop               # first run parses everything; later runs are incremental
ops knowledge import --root ~/code/shop    # if the repo already has an ops-knowledge/ export
ops watch --root ~/code/shop               # optional: keep it current while you work
```

Only one indexer runs at a time per workspace (`.ops/index.lock`), whether it's `ops index`,
`ops watch` or the MCP `reindex` tool.

Explore the result in Memgraph Lab at <http://localhost:3000>, or connect `ops-mcp`
([mcp README](../mcp/README.md)).

## Configuration

`ops` reads `ops.yaml` from `--root`, or from the nearest ancestor directory that has one. The
root [README](../../README.md#configuration--opsyaml) has the full reference. Credentials are
quoted strings in `ops.yaml` (git-ignored; share `ops.example.yaml`). Non-secret settings can use
`{ env: NAME }`, read from the process first, then from `.env`.

Logs go to stderr and to `.ops/logs/ops.YYYY-MM-DD.jsonl` in the workspace (configured under
`logging:`). The CLI writes no audit records; those are for MCP tool calls.

Embedding settings live under `semantic.embeddings` in `ops.yaml`. Provider API keys go in the
ops-support repo's `.env` before `docker compose up`, because Memgraph reads them from its own
environment. See the [root README](../../README.md#embeddings-and-semantic-search-planned).

## Development

```sh
cargo run -p cli -- --help
cargo run -p cli -- index --dry-run --root services/engine/tests/fixtures/shop   # once Part 1 lands
```
