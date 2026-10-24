<!--
ops-support incident report template, used by the `write_rca` prompt in operational-support mode.

Rules for whoever fills this in (agent or human):
- Every claim cites evidence: a tool call (name + key args) or a source location (`file:line`).
  Evidence gathered with ops-mcp is in the audit log `.ops/logs/audit.*.jsonl` for this session.
- Where evidence is missing, say so under "Open questions". Never guess a root cause.
- Times are UTC, ISO 8601.
- Never paste secrets. Tool output is already redacted; keep it that way.
- Replace every {{placeholder}}. Delete the guidance comments before publishing.
- A section with nothing to report says "None", with a reason. Don't delete it.
-->

# Incident report: {{short title, e.g. "Order payments fail with 500"}}

| Field | Value |
|---|---|
| Incident ID | {{INC-YYYYMMDD-NN}} |
| Status | {{investigating \| mitigated \| resolved \| monitoring \| closed}} |
| Severity | {{SEV1 \| SEV2 \| SEV3 \| SEV4}}: {{one-line justification}} |
| Environment | {{local \| staging \| production}} · {{k8s context / namespace, or compose project}} |
| Services affected | {{service names from `service_map`}} |
| Detected | {{timestamp}} via {{alert \| user report \| log analysis}} |
| Started (first evidence) | {{timestamp}}. Source: {{first matching log line / event}} |
| Mitigated | {{timestamp or "not yet"}} |
| Resolved | {{timestamp or "not yet"}} |
| Report author | {{name}} · ops-mcp session `{{session_id}}` |

## 1. Summary

<!-- 3–5 sentences: what broke, who was affected, the root cause in one line, what fixed it, and whether a permanent fix is in. -->
{{summary}}

## 2. Impact

| Dimension | Detail |
|---|---|
| Users / clients affected | {{who, how many, which flows (e.g. checkout from the Angular app)}} |
| Duration | {{start → mitigation; start → resolution}} |
| Functional impact | {{what users could not do}} |
| Data impact | {{none \| delayed \| lost \| inconsistent}}, plus scope and how it was verified |
| SLO / error budget | {{objective, budget burned}} |

## 3. Timeline (UTC)

| Time | Event | Source |
|---|---|---|
| {{t}} | {{trigger, e.g. deploy, config change, traffic spike}} | {{commit / deploy id / k8s event}} |
| {{t}} | {{first error observed}} | `analyze_logs(services=[…], since=…)` |
| {{t}} | {{detection}} | {{alert / report}} |
| {{t}} | {{mitigation applied}} | {{who / what}} |
| {{t}} | {{resolution verified}} | {{check from §8}} |

## 4. Symptoms

### 4.1 Observed symptoms

| # | Symptom | Where observed | First seen | Evidence |
|---|---|---|---|---|
| S1 | {{e.g. POST /api/orders/{id}/pay returns 500}} | {{frontend → orders}} | {{t}} | {{`fetch_logs(service="orders", since="1h", level="error")`}} |
| S2 | {{e.g. worker retries piling up}} | {{worker}} | {{t}} | {{…}} |

### 4.2 How to recognize this incident

<!-- The signals that identify THIS failure as opposed to a look-alike. Be exact, so on-call can match it next time. -->

| Signal | Exact form | Matches in code | How to check |
|---|---|---|---|
| Log signature | `{{log template, e.g. "payment failed for order %s"}}` | `{{file:line}}` in `{{function}}` | `match_log(line="…")` |
| Traceback / exception | `{{ExceptionType: message}}` | `{{innermost app frame file:line}}` | `match_log(line="<traceback>")` |
| Runtime state | {{e.g. CrashLoopBackOff, OOMKilled, unhealthy, restarts > N}} | n/a | `k8s_status(service=…)` / `compose_status()` |
| Datastore state | {{e.g. blocked locks, connections at max, evicted keys}} | n/a | `datastore_health(datastore=…)` |
| Config state | {{e.g. PAYMENT_GATEWAY_URL unset}} | `{{file:line}}` | `check_config(service=…)` |

**Look-alikes ruled out:**
<!-- Failures that produce similar symptoms but were excluded, and the evidence that excluded each. -->
- {{e.g. "Gateway timeout: ruled out, no timeout errors in logs during the window (`analyze_logs` …)"}}

## 5. Root cause

### 5.1 Classification

| Field | Value |
|---|---|
| Primary category | {{operational_procedure \| misconfiguration \| datastore \| code_bug \| infrastructure}} |
| Contributing factors | {{other categories and why each contributed}} |
| Confidence | {{high \| medium \| low}}: {{what would raise it}} |

### 5.2 Root cause statement

<!-- One paragraph: the specific defect or condition, where it lives, and why it produced the symptoms. -->
{{root cause}}

### 5.3 Causal chain

<!-- Trigger → fault → failure → symptom. Name the code path with trace_flow, including the boundary (http / contract / compose / shared_lib) where the failure crossed services. -->

1. **Trigger:** {{what changed or happened}}
2. **Fault:** {{the defect / misconfiguration}} at `{{file:line}}` or `{{config key, file:line}}`
3. **Propagation:** {{e.g. `onPay` (frontend) → POST /api/orders/{id}/pay → `pay` → `charge`}}, crossing boundary `{{service A → service B (via)}}`
4. **Symptom:** {{S1, S2 …}}

### 5.4 Evidence

| # | Claim | Evidence | Source |
|---|---|---|---|
| E1 | {{claim}} | {{output excerpt (redacted), or a count}} | `{{tool(args)}}` / `{{file:line}}` |
| E2 | {{claim}} | {{…}} | {{…}} |

### 5.5 Why it wasn't caught earlier

<!-- Missing test, missing check_config rule, missing alert, a procedure gap. This feeds §9. -->
{{answer}}

## 6. Validate the issue: if it happens again

<!-- A runnable checklist that confirms THIS issue is (re)occurring. Every step has a pass/fail condition. Read-only steps only. -->

| Step | Action | Issue present if | Issue absent if |
|---|---|---|---|
| V1 | `analyze_logs(services=["{{svc}}"], since="15m", min_level="error")` | cluster `{{signature}}` count > 0 | no such cluster |
| V2 | `match_log(line="{{sample line}}")` | resolves to `{{function}}` at `{{file:line}}` | resolves elsewhere / none |
| V3 | `check_config(service="{{svc}}")` | finding `{{kind}}: {{detail}}` | no such finding |
| V4 | `db_query(datastore="{{ds}}", sql="{{read-only query}}")` | {{condition on the result}} | {{condition}} |
| V5 | {{manual / user-facing check, e.g. "pay a test order in staging"}} | {{fails how}} | {{succeeds}} |

**Escalate when:** {{conditions under which on-call stops self-serving and pages an owner}}

## 7. Resolution

### 7.1 Immediate mitigation

<!-- What stops the bleeding now. Each step: who runs it, the risk, and how to roll it back. ops-mcp is read-only, so changes are made by people with the usual access. -->

| # | Step | Owner | Risk | Rollback |
|---|---|---|---|---|
| M1 | {{e.g. set PAYMENT_GATEWAY_URL in the orders deployment and restart}} | {{team}} | {{low/med/high: why}} | {{how to undo}} |

### 7.2 Permanent fix

| Type | Change | Location | Reference |
|---|---|---|---|
| {{code \| config \| data \| procedure \| infrastructure}} | {{what changes}} | `{{file:line}}` / `{{config key}}` / {{runbook}} | {{PR / ticket}} |

<!-- For code_bug: the proposed diff or a precise description of the change, plus the test that would have caught it. -->

### 7.3 Data repair

{{none, or: what was repaired, how, and how it was verified}}

## 8. Validate the resolution

<!-- Proof the fix worked and nothing regressed. Re-run §6 and expect the "absent" column. Add checks that would catch a partial fix. -->

| Check | Action | Expected | Result |
|---|---|---|---|
| R1 | Re-run V1–V5 | all "issue absent" | {{pass/fail, time}} |
| R2 | {{functional check of the affected flow}} | {{expected}} | {{…}} |
| R3 | {{boundary check, e.g. `contract(name="…")` shows no undeclared_calls}} | {{expected}} | {{…}} |
| R4 | Error rate / log volume back to baseline | `analyze_logs(since="1h")` shows no `{{signature}}` | {{…}} |

- **Monitoring window:** {{e.g. 24h. What to watch, and who watches}}
- **Sign-off:** {{name, time}}

## 9. Prevention

| # | Action | Type | Owner | Due | Ticket |
|---|---|---|---|---|---|
| P1 | {{e.g. add a test for missing PAYMENT_GATEWAY_URL at startup}} | test | {{…}} | {{…}} | {{…}} |
| P2 | {{e.g. alert on log signature `{{signature}}`}} | alert | {{…}} | {{…}} | {{…}} |
| P3 | {{e.g. runbook step: verify env before deploy}} | procedure | {{…}} | {{…}} | {{…}} |
| P4 | {{e.g. declare the endpoint in the OpenAPI contract}} | contract | {{…}} | {{…}} | {{…}} |

## 10. Runbook entry

<!-- A condensed version of §4.2 + §6 + §7.1 that on-call can follow in under 5 minutes. Copy it into the team's runbooks. -->

**When you see:** {{signal}}
**Confirm with:** {{V1, V2}}
**Mitigate by:** {{M1}}
**Verify with:** {{R1}}
**Escalate to:** {{owner}} if {{condition}}

## 11. Open questions

<!-- Anything the evidence couldn't settle. Each entry: the question, and the evidence that would settle it. -->
- {{question}}: needs {{evidence}}

## Appendix

### A. Tool calls

<!-- From `.ops/logs/audit.*.jsonl` for session {{session_id}}: tool, key args, outcome. No result content. -->
| Time | Tool | Key args | Outcome |
|---|---|---|---|
| {{t}} | {{tool}} | {{args}} | {{ok/error}} |

### B. Log excerpts (redacted)

```
{{lines}}
```

### C. Affected flow

<!-- The `trace_flow` output for the failing path, trimmed to the relevant hops. -->
```
{{flow}}
```
