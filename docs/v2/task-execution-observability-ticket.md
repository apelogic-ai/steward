# DP-D01: Durable Task execution diagnostics

Priority: post-P0

Status: bounded stdout/stderr for started OpenShell Agent attempts is implemented;
structured provider activity and pre-execution diagnostics remain deferred

## Goal

Give authorized developers and platform engineers meaningful Task logs for successful,
failed, cancelled, and pre-execution-rejected runs without treating lifecycle summaries
as execution logs or weakening Steward's credential boundary.

## Why this is separate

Steward retains bounded stdout and stderr for started OpenShell Agent attempts, exposes
live snapshots through owner/admin Run-log routes, and persists terminal streams for
successful and failed attempts. Cancelled or outcome-unknown Tasks can expose their
last live snapshot explicitly marked as partial. Rejected Tasks have no process attempt.
Tool calls and model activity are not durable Task records, so complete diagnostics
still require a retention and authorization contract beyond process output and GHA
formatting.

## Scope

- define an append-only, size-bounded Task execution-event or log-blob contract;
- retain process stdout/stderr across success, failure, cancellation, and timeout;
- record safe model-call and MCP-call metadata when authoritative providers expose it;
- distinguish advertised tools, attempted calls, denials, provider failures, and zero
  calls;
- bind records to exact `task_uid` and `runtime_uid`;
- define authenticated Task-log retrieval separate from the administrator Agent Runs
  read model;
- define retention, truncation, deletion, and sensitive-output access policy;
- return logs to `steward-run` after terminal state and expose the same source to a
  future UI timeline; and
- preserve bounded failure metadata when full diagnostics are unavailable.

## Security requirements

- never persist or return credentials, tokens, private keys, workload assertions,
  provider authorization material, or OAuth continuation data;
- provider-control executions remain structurally excluded from full Task-I/O logging;
- full task I/O is opt-in, carries the documented sensitive-output warning, and is
  readable only by an explicitly authorized Task owner or administrator;
- logs have enforced per-event, per-Task, and retention limits; and
- arbitrary output cannot inject GitHub workflow commands when replayed.

## Negative tests

- a different Task owner cannot discover or read logs;
- runtime names cannot substitute for exact runtime UIDs;
- failure persistence survives the same transaction boundaries as terminal state;
- truncation is explicit and cannot turn a failed call into apparent success;
- revoked read authority prevents subsequent retrieval; and
- no secret fixture appears in stored rows, API bodies, controller logs, or GHA output.

## Exit criteria

- an authorized caller can retrieve meaningful chronological logs for every terminal
  Task outcome;
- logs distinguish no execution, no model response, unavailable tool, denied call,
  zero attempted calls, provider failure, and bad final output;
- GHA and the future UI render the same authoritative diagnostic record; and
- the current administrator Agent Runs API remains a bounded summary rather than a raw
  log endpoint.
