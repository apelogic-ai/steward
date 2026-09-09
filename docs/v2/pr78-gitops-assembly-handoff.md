# PR #78: GitOps assembly handoff

Status: candidate preparation; not a release or deployment approval.

Source: [PR #78](https://github.com/apelogic-ai/steward/pull/78),
`feat/v2-common-core-sprint`. Lifecycle checkpoint: `5b46b58`.
Use the final pushed commit recorded in the PR for assembly, not a moving branch
name or a mixture of component revisions. No component-image digests or signed
release artifacts have been produced by this handoff yet.

## Cutoff and supplied behavior

Maintainer cutoff: fix demonstrated V2 demo blockers; document other findings in
the PR as follow-up. This does not waive mandatory gates or authorize weakened
tests. Candidate evidence and remaining gates are in the
[completion checklist](pr78-completion-checklist.md).

This PR supplies the common Task core: immutable deployment-owned execution
bindings, disposable Task provisioning, exact-UID legacy adoption, durable
admission/approval/runtime orchestration, execution ownership, and cleanup.
Public M1 callers cannot choose runtime UIDs. Legacy adopted execution detaches
without deleting the shared runtime.

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
   to reinterpret unfinished legacy work. Review migrations 0029–0032 as well:
   retirement migration 0031 fails closed on overlapping unresolved executions
   for one runtime UID.
2. Deploy all new API-server and controller replicas with
   `config.taskOrchestrationMode=staged`. Both binaries require the corresponding
   `STEWARD_TASK_ORCHESTRATION_MODE` value. New public/internal Task submissions,
   Task orchestration, and approval delivery are disabled in this stage; exact
   historical retries remain available.
3. Verify all legacy writers/controllers are gone. Install and validate the
   deployment-owned catalog and its matching native profiles. The separate
   execution-binding rollout mode must also be configured deliberately.
4. In a separate deployment change, set `config.taskOrchestrationMode=active`.
   Do not start new submissions while legacy Task controllers can still observe
   the new rows.
5. Run the selected demo path through normal authentication and admission. Verify
   exact runtime binding, model/tool access where used, output, Task finalization,
   credential retirement, and absence of Task-owned runtime resources. Legacy
   adoption must preserve its shared runtime.

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
