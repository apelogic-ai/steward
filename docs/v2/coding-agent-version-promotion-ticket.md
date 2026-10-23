# CA-P01: GitOps coding-agent release catalog and promotion

Priority: P1 post-demo

Status: proposed

Implementation owner: customer GitOps team (customer-owned GitOps repository)

## Goal

Give a customer-owned GitOps repository one reviewable process that turns an exact
upstream coding-agent version into an immutable, independently selectable Steward
`agentRef`, without changing Steward or `steward-run` source.

One release proposal is one GitOps PR. Automation materializes and verifies the
candidate before merge and writes its immutable OCI digest back to that PR. Merge is
the activation decision; it must not be the first attempt to build or validate the
image.

The normative sequence and trust boundary for this ticket are recorded in the
[GitOps coding-agent release lifecycle](coding-agent-release-lifecycle.md).

## Ownership

GitOps owns:

- the Git-backed catalog of known and enabled agent releases;
- exact upstream source or image coordinates;
- trusted compatibility-image recipes when no suitable upstream image exists;
- OCI publication to the deployment-configured registry, initially GHCR or ECR;
- pinned-image adapter conformance;
- projection into the deployment execution-binding catalog; and
- activation, rollback, and retirement.

Steward owns adapter semantics and consumes the existing
`steward.execution-bindings/v1` projection. The GitOps release record is the single
authoritative source for a promoted version; generated Helm or binding material must
not become a second manually maintained allowlist. Do not introduce another repository
or portable Task-package schema, and do not place image, executable, adapter, profile,
endpoint, registry, or credential configuration in `agentic-ops`.

## Implementation scope

- define one authoritative GitOps release record and mechanically project it into the
  deployment's existing `steward.execution-bindings/v1` catalog;
- provide an operator entry point whose only variable inputs are a supported family
  and exact version;
- keep family-to-adapter, build recipe, executable, version probe, registry selection,
  and conformance-suite selection in trusted default-branch configuration;
- materialize the candidate by copying a suitable official image or building a minimal
  compatibility image from an exact official package or binary;
- publish to a deployment-configured GHCR or ECR destination and resolve the target
  manifest digest;
- run required architecture jobs on native runners, not QEMU;
- update the same PR with the final digest, measured probe, conformance result, and
  complete binding before it can be reviewed and merged; and
- reconcile only merged records into the Steward deployment.

Implement the ordering and credential boundary defined by the lifecycle document.
Candidate publication is pre-merge; catalog activation is merge-time. A GitOps release
record may reuse compatible immutable provider profiles, and must not clone them solely
because the CLI version changed.

## Adapter conformance

The reusable lane accepts an adapter contract, exact image digest, executable, and
version probe. It uses the same candidate digest later written to the catalog. For
`claude-code-v1` it must prove, using the real pinned binary:

- exact startup and version output on every admitted architecture;
- unattended non-interactive completion without project trust or permission input;
- inference through the governed Anthropic-compatible gateway with a runtime-scoped
  token grant;
- tool-free execution without MCP attachment;
- tool-bearing execution through only the configured MCP-GW endpoint;
- at least one real allowed MCP call and rejection of a disallowed call;
- required output creation and retrieval;
- bounded stdout/stderr capture for success and failure; and
- timeout, nonzero exit, and missing-output finalization.

The first Claude promotion must also determine which executable OpenShell attributes
network activity to. The npm launcher must not cause a broad Node binary allowance
or another credential-injection bypass to be accepted without an explicit negative
proof. If the existing artifact cannot provide a narrow executable identity, use a
separate native or otherwise isolated Claude image before activation.

This conformance lane stops at the image/adapter boundary. CA-D01, rather than this
ticket, owns exact Git source resolution, User and service Envelope admission,
`steward-run`, and the complete governed GHA execution proof.

## Negative proofs

- a PR without a final immutable digest or successful conformance result cannot merge;
- a mutable image tag, unknown family/adapter, mismatched version probe, or reused
  `agentRef` with different bytes is rejected;
- untrusted PR code cannot use registry credentials or replace the privileged workflow
  or family build recipe;
- selecting ECR cannot accidentally publish to GHCR, and selecting GHCR cannot use the
  ECR workload identity;
- a failed candidate build changes no deployed execution binding; and
- retiring a binding changes no already reserved Task evidence.

## Initial implementation

- implement the generic request, candidate build/publish, PR update, validation, and
  catalog projection path in the GitOps repository;
- prove the generic path with `codex-v1` if `claude-code-v1` is not yet released;
- then promote one exact Claude Code release using `claude-code-v1` from CA-A01;
- configure or reuse an immutable Anthropic-compatible inference profile and admitted
  Claude model without coupling either to the CLI version;
- keep the existing Codex binding active; and
- make no `steward-run` change.

## Exit criteria

- one operator-supplied family and version produces a reviewable GitOps PR containing
  the final OCI digest and validated execution binding;
- GHCR and ECR destinations are selectable through trusted deployment configuration,
  with no registry credentials or selector in the Task package;
- merge, rather than image publication alone, enables the reference;
- the target environment advertises both an existing Codex reference and one exact
  Claude Code reference after the corresponding adapter is released;
- the real-image conformance lane passes natively on every admitted runner
  architecture;
- a later conforming version can be enabled through the same GitOps-only process; and
- rollback and retirement preserve already reserved Task evidence.

## Dependencies and parallel boundary

The generic workflow, registry abstraction, catalog projection, and a `codex-v1`
proof may proceed independently of CA-A01. Claude-specific conformance fixtures may
proceed once CA-A01 freezes the command contract. Claude activation requires a
released Steward containing CA-A01. CA-D01 owns only package adoption and governed
execution. Fixed local-main operation remains owned by the GitOps team, and heavy
lanes remain serialized.
