# CA-D01: Claude Code package adoption and governed execution

Priority: P1 post-demo

Status: proposed

## Goal

Prove that a repository-authored Task package can select an exact GitOps-advertised
Claude Code release and execute through the existing Steward-governed GHA path with
real inference, GitHub MCP, outputs, evidence, and diagnostics.

## Package scope

Create a new reviewed package revision in the neutral `apelogic-ai/agentic-ops`
fixtures. It may reuse the release-summary behavior, but must remain a distinct
immutable TaskDefinition version rather than silently changing an already consumed
version.

The TaskDefinition:

- sets `runtime.agentRef` to the exact Claude Code reference promoted by CA-P01;
- declares one prompt, no skills, and the existing Markdown output contract;
- declares complete explicit `requires` for the admitted Claude model, read-only
  GitHub MCP actions, budget, TTL, and Linux runner bounds;
- contains no image, executable, adapter, profile, endpoint, Envelope identity, or
  credential; and
- uses only neutral public fixtures and repository identities.

The invoking GitOps manifest continues to contain only exact Git source coordinates
and diagnostics. It does not select an Envelope or transport agent implementation
details. A new exact package commit is the only invocation change required.

## Governed execution proof

The fixed local-main E2E must demonstrate:

1. the authenticated caller submits the checked-in invocation path through the
   pinned reusable `steward-run` workflow;
2. Steward resolves the exact package closure and unique active User Envelope;
3. admission proves the explicit Claude model, GitHub tools, budget, TTL, and runner
   requirements fit the User and service Envelopes;
4. the Task evidence snapshots the exact `agentRef`, execution binding, source
   closure, effective authority, and resolved Envelope;
5. OpenShell starts the digest-pinned Claude image and verifies its exact version;
6. Claude Code makes real model calls through the governed inference gateway;
7. Claude Code makes at least one real allowed GitHub call through MCP-GW;
8. the required Markdown output is returned through `steward-run`;
9. stdout and stderr are available through the Steward run log pages; and
10. the Task and exact runtime finalize without leaked credentials or residual
    run-owned resources.

Add one focused failure case whose retained stderr makes an incorrect agent or model
binding diagnosable. Do not expand this ticket into multiple business workflows,
stable-lane coverage, publication automation, or AgentSession support.

## Exit criteria

- the package source is merged at an exact commit after normal repository review;
- its TaskDefinition selects the exact advertised Claude Code reference;
- the real governed GHA E2E succeeds with model, GitHub MCP, output, evidence, and
  diagnostic assertions;
- the existing Codex package remains executable and unchanged; and
- no Steward or `steward-run` source change is needed to switch a future package to
  another already promoted compatible Claude Code version.

## Dependencies and parallel boundary

Package authorship and fixture validation may begin once CA-P01 fixes the intended
`agentRef` and model name. Live execution waits for CA-A01 to be released and CA-P01
to activate the binding, profiles, model, and Envelope authority. The GitOps team
owns fixed local-main operation.
