# Steward documentation

Status: current documentation index

This page is the entry point for Steward documentation. Documents have different
authority and maturity; a status label is not a substitute for that distinction.

## Authority order

When documents disagree, use this order:

1. [`docs/contracts/m1/v1/**`](contracts/m1/v1/README.md) is the sole normative M1
   wire contract.
2. Current implementation documents describe behavior already present in this
   repository. Source and generated machine-readable contracts remain the final
   evidence of implementation.
3. [`docs/v2/**`](v2/README.md) records accepted post-M1 semantics and explicitly
   deferred implementation contracts. It is not an implemented or frozen wire
   contract.
4. Historical plans and design explorations are context only.

Higher-ranked sources control. No post-M1 document may reinterpret a frozen M1 field.

## Status labels

| Label | Meaning |
|---|---|
| **Frozen contract** | Normative versioned wire behavior; changes require a new compatible contract path. |
| **Current implementation** | Describes behavior present in the repository at the document's stated scope. |
| **Accepted post-M1 semantics** | Architecture direction accepted for future implementation; not a public API or claim of implementation. |
| **Deferred contract** | A wire or implementation decision that still requires separate design and acceptance. |
| **Historical** | Retained for context; not current architecture, implementation status, or contract authority. |
| **Reference** | Supporting operational or upstream material; authority is limited to its stated subject. |

## Document map

### Frozen M1 contract

| Document | Status | Purpose |
|---|---|---|
| [`contracts/m1/v1/README.md`](contracts/m1/v1/README.md) | **Frozen contract** | Entry point for `steward.m1/v1` schemas, fixtures, compatibility, and authority decisions. |
| [`m1-delivery-plan.md`](m1-delivery-plan.md) | **Historical** | Delivery dependency index for the frozen M1 outcome; not implementation status. |

### Current implementation

| Document | Status | Scope |
|---|---|---|
| [`admin-agent-runs-api-v1.md`](admin-agent-runs-api-v1.md) | **Current implementation** | Read-only administrator Agent Runs API. |
| [`admin-ui-contract-v1.md`](admin-ui-contract-v1.md) | **Current implementation** | Browser presentation and API boundary. |
| [`browser-session-contract-v1.md`](browser-session-contract-v1.md) | **Current implementation, with stated activation dependency** | Browser authentication and session boundary. |
| [`canonical-user-identity-v1.md`](canonical-user-identity-v1.md) | **Current implementation** | Canonical person identity and ownership keys. |
| [`github-actions-generator.md`](github-actions-generator.md) | **Current implementation, renderer scope** | Bounded GitHub Actions YAML generation. |
| [`installation/execution-bindings.md`](installation/execution-bindings.md) | **Current implementation** | Deployment-neutral coding-agent catalog, validation, Helm installation, and lifecycle behavior. |
| [`installation/upgrade-execution-bindings.md`](installation/upgrade-execution-bindings.md) | **Current implementation** | Upgrade from implicit coding-agent behavior to explicit deployment bindings. |
| [`task-submission-api.md`](task-submission-api.md) | **Current pre-M1 compatibility behavior** | Implemented v0.1 Task API; subordinate to the frozen contract for M1. |

### Accepted post-M1 direction

| Document | Status | Purpose |
|---|---|---|
| [`v2/README.md`](v2/README.md) | **Accepted post-M1 semantics** | Canonical terminology, execution model, lifecycle, and ownership boundaries. |
| [`v2/deferred-implementation-contracts.md`](v2/deferred-implementation-contracts.md) | **Deferred contract** | The one current register of unresolved post-M1 wire and implementation contracts. |
| [`v2/documentation-reconciliation-ticket.md`](v2/documentation-reconciliation-ticket.md) | **Accepted post-M1 semantics (decision record)** | Requirements and rationale that established this documentation baseline. |

### Historical design and planning

| Document | Status | Context |
|---|---|---|
| [`solution-overview.md`](solution-overview.md) | **Historical** | Original long-running-agent product exploration; uses superseded object boundaries. |
| [`steward-ai-workflows-fit.md`](steward-ai-workflows-fit.md) | **Historical** | Earlier workload-mapping analysis; uses the unadopted multi-step Workflow model. |
| [`data-plane-spec.md`](data-plane-spec.md) | **Historical** | Earlier multi-step data-plane exploration. |
| [`workflow-and-task-spec.md`](workflow-and-task-spec.md) | **Historical, superseded** | Redirect from the unadopted Steward Workflow proposal. |
| [`roadmap/steward-roadmap.md`](roadmap/steward-roadmap.md) | **Historical** | Original v0.1 roadmap; not a report of current implementation. |
| [`guarantee-register-generation.md`](guarantee-register-generation.md) | **Historical design note** | Earlier proposal for generated conformance status. |

### References and artifacts

| Location | Status | Purpose |
|---|---|---|
| [`upstream/openshell-upstream-strategy.md`](upstream/openshell-upstream-strategy.md) | **Reference** | Versioned upstream findings and engagement record. |
| [`upstream/pr-1970-review-comment.md`](upstream/pr-1970-review-comment.md) | **Historical, superseded** | Withdrawn upstream review draft. |
| [`upstream/rfc-0011-review-comment.md`](upstream/rfc-0011-review-comment.md) | **Historical reference** | Retained upstream review text. |
| [`examples/github-file-read-v1.yaml`](examples/github-file-read-v1.yaml) | **Example** | Example generated workflow; its owning contract states its authority. |
| [`diagrams/`](diagrams/) | **Historical illustrations** | Images associated with the earlier architecture documents; not contracts. |
