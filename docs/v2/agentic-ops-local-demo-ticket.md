# P0: Customer-owned agentic-ops Task executes through GHA on local-main

Status: repository and trust-policy preparation complete; live compatibility-path
publication and rehearsal remain unverified

## Outcome

A customer DevInt/DevOps role can inspect one pre-created, pre-approved artifact in
agentic-ops, dispatch an ordinary GitHub Actions workflow, and observe Steward execute
its instructions under the configured authority, return a useful result, and finalize
the disposable runtime. Approval ceremonies happen during preparation.

## Scope and implementation truth

The private `apelogic-ai/agentic-ops` repository now contains one review task, portable
source, validation, an authoring guide, and a pinned `steward-run` caller. Current
`main` is `73306cefb4fe12995fcbda29f8e35f1da5bec129`; the selected compatibility package
uses `agentic-release-integration-review@2` and `codex@0.140.0`. Record the exact source
again at rehearsal time because a later commit requires deliberate revalidation.

The initial supported compatibility path publishes agent/prompt as name@version.
It derives execution requirements from the approved User Envelope. Manifest validation
alone does not establish runtime enforcement of portable requirements, full dependency
closure resolution, M1 publication witnesses, or signed M1 evidence. The demo must label
these limits and never silently drop a dependency or requirement it claims to execute.
Preflight verifies that the prepared Envelope/binding covers the intended task and that
the published agent/prompt matches the approved repository bytes. Record any supported
derivation explicitly, including source and resulting content digests.

## Start and integration gates

- PR #78 merged as `c87bece81f2324d83c1177a10be56ae8ceb7111a` and supplies the
  common Task application and orchestration core. GitOps still selects and verifies a
  compatible released assembly; merge alone is not deployment readiness.
- GitOps owns the retained local-main cluster. Development workers must not use it
  for testing, diagnosis, recovery, or deployment. Hand deployment and readiness
  requirements to GitOps; do not restart or reset the cluster from this workstream.
- Record the exact local Steward, Identity, steward-run workflow/action, OpenShell,
  MCP-GW, LiteLLM, and native binding revisions before integration.
- The prepared demo remains on the existing compatibility path until the proposed
  [direct-package architecture](direct-package-task-invocation.md) is approved,
  implemented, and passes its own E2E. Do not change the attended demo contract merely
  because the architecture proposal exists.
- Preserve existing worktrees and retained local state. Development integration tests
  use isolated, run-owned environments under the local testbed skill. Only the agreed
  demo handoff uses GitOps-managed local-main, with explicit ownership and context.

## Deliverables

1. Deliver the exact component pins and readiness checklist to GitOps. GitOps prepares
   local-main through its lifecycle workflow and confirms control plane/networking,
   identity, model/tool gateways, native ARM64 sandbox, and Steward API/controller/UI.
   Run development smoke and integration checks in isolated environments first.
2. Create the repository and one useful package with neutral demonstration inputs,
   a reproducible validator, a documented customer authoring boundary, and exact pins.
   Prepare changes through PR review; merge and protection changes remain human actions.
3. Make a dedicated labelled self-hosted runner available to this repository. Existing
   registration to GitOps does not imply access from agentic-ops. Keep local-only
   authority and one-at-a-time execution across all callers sharing the host.
4. Register the exact caller repository ID, owner ID, workflow/ref/event and reusable
   workflow identity. Configure non-secret URLs, CA references, audience and runner
   variables outside portable artifacts. Verify the effective Identity mapping.
5. Provision the intended canonical user's Envelope and provider connection through
   supported interfaces. Keep model credentials in approved local secret storage.
6. For the compatibility demo, pre-publish the approved artifact through the supported
   Steward interface and verify agent/prompt/version/digest against repository source.
   If direct-package invocation becomes an explicit demo requirement later, replace this
   step only after the new contract and full path pass isolated E2E.
7. After GitOps confirms demo readiness and agrees the rehearsal window, dispatch the
   normal pinned steward-run workflow from agentic-ops. It must upload
   the intended input, execute a real governed model/tool task, return structured
   summary.md, expose Task correlation, and finalize correctly on success or failure.
8. Complete two successive rehearsals against unchanged pins, with useful output,
   evidence of an authorized MCP call and model inference, and exact runtime cleanup.
9. Prepare a short presentation sequence: repository artifact, GHA dispatch, Steward
   Task/authority view, result, cleanup. Retain a sanitized rehearsal result as fallback.

## Exit criteria

- The new repository is accessible with the approved artifact and executable caller.
- Compatibility publication mapping is verified; declared but unenforced future fields
  are explicit. This is not direct-package or frozen-M1 catalog evidence.
- Two real agentic runs succeed through the normal GHA path on recorded component pins.
- Each finalized Task leaves no disposable Task-owned AgentRuntime, Sandbox, runtime
  Secret, or model/tool authority projection. Failed runs receive bounded finalization.
- Setup/test credentials are revoked during their teardown. Any retained demo connection,
  local service, runner or model configuration has an explicit owner, purpose, expiration
  or cleanup time, and exact scoped cleanup command; do not revoke unrelated connections.
- The local-main demo remains available for the attended presentation under that recorded
  retention plan. Required repository checks pass; no claim is made for unrun M1 tests.

## Parallel ownership and coordination

- P0 artifact owner: agentic-ops initial package, validator, caller, authoring README.
- P0 integration coordinator: repository/runner preparation, exact Identity change
  handoff, publication mapping, isolated development checks, and demo coordination.
- GitOps team: local-main state, deployment, recovery, readiness, and the agreed demo
  execution window. Agree publication, credentials, dispatch, evidence, and cleanup
  ownership before rehearsal; development workers do not operate this cluster.
- Direct-package planning: architecture and contract work remains separate from the
  fixed compatibility rehearsal until an explicit migration decision.
- Ticket A: optional catalog/release tooling; it does not block direct invocation or P0.
- Ticket B: direct-package fixtures/harnesses and contract matrix on isolated branches;
  request a test slot before heavy builds, deployments or GHA runs.

Keep one writer for each shared file and one coordinator for isolated test scheduling.
Shared local-main environment changes remain with GitOps, not the development workers.
Status questions do not interrupt workers. Report progress and blockers periodically;
pause only dependent work when a real prerequisite is absent and continue independent
work. Request human merge or required approval only with concrete reviewable changes.

## Deferred work

[Ticket A](agentic-ops-publication-lifecycle-ticket.md) now owns optional catalog
promotion and release provenance. [Ticket B](customer-authored-task-conformance-ticket.md)
owns direct-package authority/isolation and released stable-lane proof. Neither changes
the prepared compatibility demo without an explicit migration decision.
AgentSession, AgentInstance, TaskGraph, DEV readiness, multiple tasks, and live approval
ceremonies are outside P0.
