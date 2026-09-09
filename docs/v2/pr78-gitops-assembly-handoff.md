# PR #78: GitOps assembly handoff

Status: V2-only source candidate verified for assembly. Not deployment approval.
The maintainer explicitly retired legacy adoption / execute-and-detach from this
demo's acceptance scope after run `task-20260909065936-32809` returned HTTP 409.
That compatibility defect is not claimed fixed. The remaining full V2 Task E2E
passed in run `task-20260909072600-79082` (256.01s), and full `cargo xtask ci`
passed on `61b9af9`, including all pinned guarantees. See the completion checklist
for earlier real-Postgres, governed-connections and adapter evidence.

Source: [PR #78](https://github.com/apelogic-ai/steward/pull/78),
`feat/v2-common-core-sprint`. Tested code/test checkpoint: `61b9af9`.
Use the final pushed commit recorded in the PR for assembly, not a moving branch
name or a mixture of component revisions. No component-image digests or signed
release artifacts have been produced by this handoff yet.

The persistent `.worktrees/v2-core` lane remains for this PR; it is not a retained
test environment. All run-owned clusters, probes and credential directories from
the final Task/CI runs were removed. Other worktrees and foreign workloads were
left untouched.

## Cutoff and supplied behavior

Maintainer cutoff: fix demonstrated V2 demo blockers; document other findings in
the PR as follow-up. This does not waive mandatory gates or authorize weakened
tests. Candidate evidence and remaining gates are in the
[completion checklist](pr78-completion-checklist.md).

This PR supplies the common Task core: immutable deployment-owned execution
bindings, disposable Task provisioning, durable
admission/approval/runtime orchestration, execution ownership, and cleanup.
Public M1 callers cannot choose runtime UIDs. Do not assemble this demo around
legacy runtime adoption: execute-and-detach is no longer an E2E-verified handoff
claim. Existing shared-runtime identity and cleanup protections remain in code.

It does **not** implement resident Task dispatch, AgentInstance/AgentSession APIs,
or the later common event/UI and GitHub Actions publication lanes. A demo requiring
those features must compose the corresponding lane deliverables; a resident
binding in this PR is not an executable resident agent.

## Assembly inputs

The existing [release workflow](../../.github/workflows/release.yml) is the build
and provenance source of truth. Do not substitute local test images for signed
release artifacts.

| Component | Build input |
|---|---|
| API server | `build/package.Dockerfile`, `BINARY=steward-apiserver-bin` |
| Controller | `build/package.Dockerfile`, `BINARY=steward-controller-bin` |
| Mint | `build/package.Dockerfile`, `BINARY=steward-mint-bin` |
| Connections bridge, when used | `build/connections-bridge.Dockerfile` |
| Web, when used | `build/web.Dockerfile` |
| Helm chart | `charts/steward` from the same source revision |
| Runtime provider bundle | `scripts/package-provider-profile-bundle.sh`; currently packages `steward-runtime-providers@1.2.0` |

The release workflow builds `linux/amd64` component images. Local Apple Silicon
OpenShell agent verification needs native `linux/arm64` sandbox derivatives;
those are test inputs, not DEV release replacements.

Deployment owns the execution-binding catalog: logical agent reference, exact
image digest, supported adapter, executable/version probe, and exact provider
profile IDs/digests. Install matching profiles before activating the catalog.
The packaged 1.2.0 profiles list the older Codex `.../codex/codex` executable
paths. Do not assume they authorize Codex 0.140's `.../bin/codex` layout. A demo
selecting 0.140 requires a compatible immutable profile and matching catalog
binding; ad-hoc two-version E2E profiles do not prove packaged-bundle compatibility.

Use existing Secret references and the normal Identity, workload exchange,
OpenShell, MCP-GW, LiteLLM, and decision-channel configuration. No credentials,
kubeconfigs, or local test artifacts belong in GitOps source.

## Required rollout order

1. Drain and finalize legacy Tasks before migration 0028. It intentionally refuses
   to reinterpret unfinished legacy work. Review migrations 0029–0033 as well:
   retirement migration 0031 fails closed on overlapping unresolved executions
   for one runtime UID.
   Migration 0033 retires consumed internal connection payloads while preserving
   immutable execution evidence. Replace old process connections during the
   staged rollout; populated upgrade/restart is covered by the store tests.
2. Deploy all new API-server and controller replicas with
   `config.taskOrchestrationMode=staged`. Both binaries require the corresponding
   `STEWARD_TASK_ORCHESTRATION_MODE` value. New public/internal Task submissions,
   Task orchestration, and approval delivery are disabled in this stage; exact
   historical retries remain available.
3. Verify all legacy writers/controllers are gone. Install and validate the
   deployment-owned catalog and its matching native profiles. The separate
   execution-binding rollout mode, `config.apiserver.executionBindingsMode`,
   must also be configured deliberately; it defaults to `staged` and is separate
   from the Task orchestration switch.
4. In a separate deployment change, set `config.taskOrchestrationMode=active`.
   Do not start new submissions while legacy Task controllers can still observe
   the new rows.
5. Run the selected demo path through normal authentication and admission. Verify
   exact runtime binding, model/tool access where used, output, Task finalization,
   credential retirement, and absence of Task-owned runtime resources. Legacy
   runtime adoption is outside this demo's accepted scope.

An `outcome_unknown` Task does not free a shared runtime for reuse. Quarantine
survives Task finalization and restart; only exact retirement evidence releases
the execution lease. See the
[orchestration architecture](../task-runtime-orchestration.md) for recovery.

## Handoff boundary

Human review/merge and release publication remain separate from this branch
handoff. GitOps assembly must record the selected source commit, published image
and chart digests, provider-bundle identity/digest, execution-binding catalog,
and selected demo lane. No DEV environment was deployed or changed by this work.
Non-demo follow-up must remain visible in the PR rather than silently expanding
this implementation slice.
