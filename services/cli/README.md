# cli: `ops`

The command-line entry point to ops-support. `ops` sets a workspace up and keeps its graph in
Memgraph current: it discovers services and writes their configuration, checks that credentials
and backends are reachable, and indexes the code and infrastructure files into the graph that
`ops-mcp` queries.

> [!NOTE]
> **Status: stub.** The binary builds but has no commands yet. They're added by
> [Part 1 of the plan](../../docs/superpowers/plans/2026-10-24-ops-indexer.md) (Tasks 3 and 7).
> All logic lives in [`engine`](../engine/README.md); this crate only parses arguments and prints
> results.

## Commands *(planned)*

| Command | What it does |
|---|---|
| `ops init [--root DIR] [--force]` | Discovers services, shared libraries, API contracts and datastores, then writes `ops.yaml`, `.env.example` and `.mcp.json`. Never overwrites an existing file without `--force`. |
| `ops check [--root DIR]` | Validates `ops.yaml`, names every credential env var that isn't set, and checks that Memgraph, the datastores and (if configured) the kubectl context are reachable. Exits 1 if anything fails. |
| `ops index [--root DIR] [--dry-run]` | Parses the workspace and replaces its graph in Memgraph; re-running gives the same result. `--dry-run` prints node and edge counts without touching Memgraph. |

Global flags: `-v` / `-q` raise or lower log verbosity. The `OPS_LOG` env var overrides both
(e.g. `OPS_LOG=engine=debug`).

## Typical flow

```sh
docker compose up -d                       # from the repo root: Memgraph + Lab
ops init  --root ~/code/shop               # review the generated ops.yaml
cp ~/code/shop/.env.example ~/code/shop/.env && $EDITOR ~/code/shop/.env
ops check --root ~/code/shop
ops index --root ~/code/shop
```

Explore the result in Memgraph Lab at <http://localhost:3000>, or connect `ops-mcp`
([mcp README](../mcp/README.md)).

## Configuration

`ops` reads `ops.yaml` from `--root`, or from the nearest ancestor directory that has one. The
root [README](../../README.md#configuration--opsyaml) has the full reference. Credentials are
literals or `{ env: NAME }`; env vars come from the process first, then from `.env`.

Logs go to stderr and to `.ops/logs/ops.YYYY-MM-DD.jsonl` in the workspace (configured under
`logging:`). The CLI writes no audit records; those are for MCP tool calls.

## Development

```sh
cargo run -p cli -- --help
cargo run -p cli -- index --dry-run --root services/engine/tests/fixtures/shop   # once Part 1 lands
```
