# mcp: `ops-mcp`

A stdio [MCP](https://modelcontextprotocol.io) server that turns an MCP client (Claude Code,
Claude Desktop, …) into an operational-support agent for your workspace. Given a problem statement,
the client uses its tools to read logs across services, map them onto the code flow in the
Memgraph graph, check configuration and datastores, and write an incident report with a root
cause and a fix.

> [!NOTE]
> **Status: stub.** Built by [Part 2 of the plan](../../docs/superpowers/plans/2026-10-24-ops-mcp.md).
> All logic lives in [`engine`](../engine/README.md); this crate adapts it to MCP with `rmcp` 3.7.

> [!IMPORTANT]
> No LLM runs inside `ops-mcp`. The client is the agent. The server provides tools, agent
> playbooks (MCP prompts) and instructions.

## Running *(planned)*

```sh
ops-mcp --root /path/to/workspace --mode operational-support
```

| Flag | Default | Meaning |
|---|---|---|
| `--root` | `$OPS_ROOT`, then the current directory | Workspace containing `ops.yaml` |
| `--mode` | `operational-support` | Which bundle of tools, prompts and instructions to serve |

Register it with Claude Code:

```sh
claude mcp add ops -- ops-mcp --root /path/to/workspace --mode operational-support
```

or use the `.mcp.json` that `ops init` writes. GitHub Copilot CLI connects to the same stdio
server; `ops init` writes or prints its MCP config snippet. Both clients share one graph and one
semantic layer, and each knowledge entry records which client and model wrote it.

The workspace must be indexed first (`ops index`). The server still starts without Memgraph or
any datastore; affected tools report the failure as text.

## Modes

| Mode | For | Status |
|---|---|---|
| `operational-support` | Incidents: logs → code flow → root cause → fix | **MVP** |
| `developer-support` | Context for code changes from the service graph, with your LSP setup, context files, prompts and hooks | later |
| `operational-onboarding` | Do's and don'ts for operating the system; no production-access tools | later |
| `developer-onboarding` | A guided tour of the codebase | later |

One mode per process. To use several, register the server several times.

## Tools: `operational-support` *(planned)*

| Group | Tool | Purpose |
|---|---|---|
| Graph and code | `service_map` | Services, datastores, and boundaries between services with evidence (contract, shared lib, compose, http) |
| | `contract` | A contract's endpoints and implementations, the schemas and their users, and undeclared calls |
| | `find_symbol` / `code_context` | Locate a function or class; get its source, callers, callees, routes, logs and env reads |
| | `trace_flow` | Call paths up to entrypoints and routes, or down to leaves, across service boundaries |
| | `read_file` | Read code inside configured paths only |
| | `graph_query` | Read-only Cypher |
| | `lsp` | Definition, references and hover from the service's language server |
| Operations | `fetch_logs` | Logs from compose, Kubernetes (allow-listed namespaces), files or a command |
| | `match_log` | Log line or traceback → the function that emitted it, and its caller chain |
| | `analyze_logs` | Cluster warnings and errors across services and map each cluster to code |
| | `compose_status` / `k8s_status` | Container and pod state: restarts, OOMKilled, CrashLoopBackOff, warning events |
| | `service_config` / `check_config` | Redacted config view; findings such as missing env vars or port mismatches, with `file:line` |
| Data | `db_query` / `db_schema` | Read-only PostgreSQL |
| | `redis_command` | Allow-listed read-only Redis commands |
| | `datastore_health` | Connections, long queries and locks (Postgres); memory, evictions and clients (Redis) |
| Semantic layer | `recall` | Knowledge about a node and its neighbours, or search by meaning (vector + text) |
| | `search_code` | Find code by meaning ("where do we take payment"), with `match_lines` pointing to the matching part of long functions |
| | `knowledge_gaps` | Important code with no summary yet, plus stale and orphaned entries |
| | `remember` / `revise` / `retract` | Record, update (keeping history) or withdraw an entry: summary, concept, flow, relation, invariant, gotcha, ops note, always with evidence and confidence |
| Index | `reindex` | Incremental re-index on request; reports "index in progress" if another indexer runs |
| Incidents | `find_incident_reports` | Search past reports by service, category or text; the first reference in `diagnose` |
| | `save_incident_report` | Save a finished report to `incidents.dir` (default `docs/incidents/`), never overwriting |

## Prompts (agents)

| Prompt | Use it to |
|---|---|
| `diagnose(problem)` | Run the full investigation, starting from past incident reports and recalled knowledge. Classify the root cause (operational procedure, misconfiguration, datastore, code bug or infrastructure), and record durable facts learned along the way |
| `triage_logs(service)` | Summarize what's going wrong in one service's logs |
| `explain_flow(entry)` | Walk a route, function or UI event through the code |
| `check_misconfiguration(service)` | Hunt for config and environment problems |
| `investigate_datastore(datastore, symptom)` | Dig into a Postgres or Redis symptom |
| `write_rca(findings)` | Produce an incident report from [`templates/incident-report.md`](templates/incident-report.md), then save it with `save_incident_report` |
| `enrich_knowledge(scope)` | Work through `knowledge_gaps`: read the code, then `remember` summaries, flows, relations and gotchas, or `revise` stale entries |

The incident report covers symptoms and how to recognize them, root cause with a causal chain and
evidence, a checklist to validate a recurrence, mitigation and a permanent fix, a checklist to
validate the resolution, prevention, and a five-minute runbook entry.

## Safety and audit

- Tool output and errors are redacted, and secrets are shown only as "set" or "not set".
- Postgres access is read-only at the database level, with timeouts and row caps. Redis accepts
  only read commands.
- `read_file` and `lsp` are limited to configured paths. `.env*`, `.ops/` and `ops.yaml` are
  always denied.
- Kubernetes access is limited to `kubernetes.namespaces`. Arguments are never passed through a
  shell.
- Writes to Memgraph happen only through `remember` / `revise` / `retract` (schema-checked, size-capped,
  redacted) and `reindex`. `graph_query` stays read-only, and knowledge is never hard-deleted.
- stdout carries only MCP protocol messages. Diagnostics go to stderr and `.ops/logs/`.
- **Every tool call is audited** to `.ops/logs/audit.YYYY-MM-DD.jsonl`: tool, redacted arguments,
  outcome, duration, backends touched and result size. Result content is never recorded.

## Development

```sh
cargo test -p mcp      # includes stdio smoke tests that run with every backend down
```

Prompt playbooks will live in `src/prompts/*.md`, and the incident template in
`templates/incident-report.md`. Both are compiled in with `include_str!`.
