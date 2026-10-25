# GitOps coding-agent release lifecycle

Status: accepted architecture direction; implementation pending

This document defines how a customer-owned GitOps repository becomes the single
Git-backed catalog of coding-agent releases available to its Steward deployment. It
does not change the frozen `steward.m1/v1` contract.

## Decision

A coding-agent family has one stable Steward adapter contract, such as `codex-v1` or
`claude-code-v1`. Each compatible product version is enabled by one reviewed GitOps
catalog PR. Steward source and `steward-run` do not change for another version that
satisfies an existing adapter contract.

The GitOps catalog is the deployment allowlist. A package selects only its exact
logical reference:

```json
{
  "runtime": {
    "agentRef": "claude-code@2.1.230"
  }
}
```

The package never selects an image, registry, executable, adapter, provider profile,
endpoint, credential, or Envelope. GitOps projects the merged release record into the
existing `steward.execution-bindings/v1` document, and Steward snapshots the resolved
binding into each admitted Task.

## Two meanings of a new agent

- A new version compatible with an existing adapter is GitOps-only work.
- A new family or incompatible command/configuration protocol first requires one new
  Steward adapter contract. After that adapter is released, its compatible versions
  follow the same GitOps-only lifecycle.

Version-specific conditionals or allowlists must not accumulate in Steward production
code.

## Single source of truth

The customer GitOps repository contains one authoritative record for each promoted
release. Its concrete internal schema belongs to that repository, but the completed
record must determine:

- exact immutable `agentRef`;
- stable Steward adapter contract;
- exact official upstream package, binary, or image source;
- trusted family build recipe when a compatibility image is required;
- target OCI image repository and immutable manifest digest;
- absolute executable and exact version probe;
- reusable immutable tool and inference profile references required by the adapter;
  and
- the conformance result for the exact candidate digest.

Helm values and `steward.execution-bindings/v1` are generated or mechanically
projected from that record. They must not be maintained as independent agent-version
lists. Provider profiles are separate policy artifacts: a compatible release reuses
them, while a policy change creates a new immutable profile ID and digest.

The OCI registry is deployment configuration. The first implementation supports GHCR
and ECR. Task packages and invocation manifests remain registry-independent.

## Promotion lifecycle

```text
catalog PR
  -> trusted candidate workflow
  -> exact OCI digest + native conformance
  -> automation updates the same PR
  -> human review
  -> merge enables the agentRef
  -> GitOps reconciliation
  -> Steward advertises and resolves it
```

1. An operator opens a GitOps PR requesting a supported family and exact upstream
   version.
2. Unprivileged PR checks validate the request shape and reject mutable versions,
   unknown families, arbitrary commands, and caller-selected build recipes or
   registries.
3. An authorized promotion workflow executes the workflow and family recipe from the
   trusted default branch. PR-controlled code does not run with registry credentials.
4. The workflow materializes an image in the configured registry. It may copy a
   suitable official upstream image or build a minimal compatibility image from an
   exact official package or binary.
5. Native runners build or verify every architecture enabled by the deployment. QEMU
   is not a supported release proof.
6. The workflow resolves the target registry's immutable manifest digest, verifies the
   exact executable/version probe, and runs the stable adapter conformance suite against
   that digest.
7. Automation writes the final digest, probe, conformance result, and complete
   execution binding into the same PR. A candidate artifact in a registry is not yet
   enabled.
8. Required checks validate the completed record using the released Steward binding
   parser. Reviewers approve the exact artifact and binding that will be deployed.
9. Merge activates the record. Normal GitOps reconciliation updates the deployment's
   execution-binding catalog; Steward then advertises the new `agentRef` for new Tasks.

Building for the first time after merge is intentionally excluded. That ordering
would make an unbuilt or nonconforming release part of desired state and would prevent
reviewers from approving the actual digest. Merge is the enablement boundary, not the
start of candidate production.

## Workflow trust boundary

The privileged promotion job is restricted to authorized maintainers and an approved
GitHub Environment or equivalent control. It reads its workflow, build recipe, base
image policy, registry destination, adapter mapping, executable path, and probe format
from the trusted default branch. The PR supplies only bounded data such as family and
exact version.

GHCR publication may use narrowly scoped GitHub permissions. ECR publication uses the
deployment's configured workload identity. Neither credential is exposed to ordinary
pull-request jobs, forks, built images, Steward Tasks, or agentic packages.

## Conformance boundary

Conformance proves that the exact image digest implements its declared stable adapter,
including:

- exact executable and version output;
- unattended completion without interactive trust or permission prompts;
- governed inference and, when configured, governed MCP transport;
- absence of ambient MCP/configuration discovery prohibited by the adapter;
- required result and output handling;
- bounded success and failure diagnostics; and
- timeout and nonzero-exit finalization.

Agent-specific assertions belong to the stable adapter suite. The promotion workflow
selects the suite by the trusted family mapping; a release proposal cannot choose a
weaker suite.

## Runtime and audit behavior

Multiple exact versions may remain enabled simultaneously. New Tasks resolve the
currently deployed binding for their selected `agentRef`; already reserved Tasks keep
their immutable binding evidence when the catalog changes.

Retirement is another reviewed GitOps PR. Removing a binding prevents new Tasks from
selecting it but does not rewrite historical or reserved Task evidence. Rollback
restores a previously reviewed catalog record and immutable artifact; an `agentRef`
is never repointed to different bytes.

## Initial delivery and deferred hardening

The initial delivery is complete when the generic GitOps workflow can promote a
version through one PR, target either GHCR or ECR through trusted deployment
configuration, validate the resulting binding, and activate it on merge. It can prove
the generic machinery with `codex-v1` while `claude-code-v1` is developed.

Signing policy, SBOM attestation, provenance retention, registry replication,
garbage-collection protection, multi-environment promotion, and automated retirement
impact analysis are recorded follow-up hardening. They must not create a second
catalog or move version selection into Steward code.

## Work split

- [CA-A01](coding-agent-adapter-ticket.md) adds `claude-code-v1` once in Steward.
- [CA-P01](coding-agent-version-promotion-ticket.md) implements this generic lifecycle
  in the customer-owned GitOps repository.
- [CA-D01](claude-code-package-enablement-ticket.md) adopts the promoted exact
  `agentRef` from `agentic-ops` and proves real governed GHA execution.

CA-P01's generic workflow and Codex proof can proceed in parallel with CA-A01. Claude
activation requires CA-A01; package fixture work can proceed once the intended
`agentRef` and model are known. Fixed local-main testing remains GitOps-owned, and
heavy lanes remain serialized.
