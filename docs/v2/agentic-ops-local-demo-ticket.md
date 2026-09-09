# P0: Customer-owned agentic-ops Task executes through GHA on local-main

Status: ready to start; highest priority; no blanket dependency on PR #78 release

## Outcome

A customer DevInt/DevOps role can inspect one pre-created, pre-approved artifact in
agentic-ops, dispatch an ordinary GitHub Actions workflow, and observe Steward execute
its instructions under the configured authority, return a useful result, and finalize
the disposable runtime. Approval ceremonies happen during preparation.

## Scope and implementation truth

Create apelogic-ai/agentic-ops with one review task, portable source, validation, an
authoring guide, and a pinned steward-run caller. Record the repository commit and
content-to-publication mapping. Use a reviewed private repository by default.

The initial supported compatibility path publishes agent/prompt as name@version.
It derives execution requirements from the approved User Envelope. Manifest validation
alone does not establish runtime enforcement of portable requirements, full dependency
closure resolution, M1 publication witnesses, or signed M1 evidence. The demo must label
these limits and never silently drop a dependency or requirement it claims to execute.
Preflight verifies that the prepared Envelope/binding covers the intended task and that
the published agent/prompt matches the approved repository bytes. Record any supported
derivation explicitly, including source and resulting content digests.

## Start and integration gates

- Repository creation, artifact authoring, workflow preparation, and local-main
  diagnosis can start immediately while #78 is tested.
- Record the exact local Steward, Identity, steward-run workflow/action, OpenShell,
  MCP-GW, LiteLLM, and native binding revisions before integration.
- Prefer the existing compatible execution path when sufficient. Adoption of #78
  requires a fixed candidate with relevant gates passing, including legacy transport
  and lifecycle regression tests; a candidate demo does not establish release readiness.
- User updates about #78 trigger a compatibility assessment, not an automatic rollout.
- Preserve existing worktrees and retained local state. Follow repository runbooks and
  the local testbed skill using explicit local-main kubeconfig/context and ownership.

## Deliverables

1. Recover or provision local-main through the approved local lifecycle workflow.
   Verify control plane/networking, identity, model/tool gateways, native ARM64 sandbox,
   Steward API/controller/UI, and the existing deterministic/model smoke ladder.
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
6. Pre-publish the approved artifact through the actual supported Steward interface.
   Read it back and verify agent/prompt/version/digest against the repository source.
7. Dispatch the normal pinned steward-run workflow from agentic-ops. It must upload
   the intended input, execute a real governed model/tool task, return structured
   summary.md, expose Task correlation, and finalize correctly on success or failure.
8. Complete two successive rehearsals against unchanged pins, with useful output,
   evidence of an authorized MCP call and model inference, and exact runtime cleanup.
9. Prepare a short presentation sequence: repository artifact, GHA dispatch, Steward
   Task/authority view, result, cleanup. Retain a sanitized rehearsal result as fallback.

## Exit criteria

- The new repository is accessible with the approved artifact and executable caller.
- Publication mapping is verified; declared but unenforced future fields are explicit.
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
- P0 integration coordinator: local-main state, Identity deployment, runner setup,
  publication, credentials, GHA dispatch, readiness, rehearsal evidence, cleanup.
- Ticket A: publication/release tooling in isolated paths and branches; agree on the P0
  package interface before editing shared artifacts. No automatic demo-stack upgrades.
- Ticket B: additional fixtures/harnesses and contract matrix on isolated branches;
  request a test slot before heavy builds, deployments or GHA runs.

Keep one writer for each shared file and one coordinator for shared environment changes.
Status questions do not interrupt workers. Report progress and blockers periodically;
pause only dependent work when a real prerequisite is absent and continue independent
work. Request human merge or required approval only with concrete reviewable changes.

## Deferred work

[Ticket A](agentic-ops-publication-lifecycle-ticket.md) owns automated publication and
release provenance. [Ticket B](customer-authored-task-conformance-ticket.md) owns broader
authority/isolation and released stable-lane proof. Neither blocks this demo.
AgentSession, AgentInstance, TaskGraph, DEV readiness, multiple tasks, and live approval
ceremonies are outside P0.
