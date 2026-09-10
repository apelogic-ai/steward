# Direct immutable package Task invocation

Status: proposed architecture and implementation plan; approval required before wire,
schema, or enforcement changes

## Decision to approve

A ratified GitHub Actions caller may invoke one `TaskDefinition` directly from an
immutable package in its own authorized repository. A separate Steward catalog
publication is not a prerequisite for execution.

The author-facing reference is the exact Git source identity plus package path. Steward
validates the package, computes its complete closure, persists an immutable resolution,
and passes the resulting plan through the common Task application and orchestration
implemented by PR #78. Catalog publication remains an optional distribution and
curation layer.

This proposal does not change the frozen `steward.m1/v1` contract. It requires a new
versioned direct-source Task contract. The provisional name `steward.task/v2` is used
below only so the shapes are readable; contract approval freezes the actual identifier.

## Why this boundary

For an `agentic-ops` repository, reviewed Git source is already the customer-owned
authoring and approval boundary. Requiring another administrator to copy the same
content into Steward adds ceremony without adding authority when the deployment has
already decided to trust that exact repository and GHA caller.

Integrity and authority remain separate:

- repository ID, commit SHA, package path, and closure digest identify what was run;
- Identity policy establishes which caller and source Steward trusts;
- a server-issued Envelope reference and expected digest select the approved authority
  ceiling for this invocation; and
- Steward admission decides whether the resolved package fits that currently active
  Envelope and the deployment-owned execution binding.

A digest alone grants nothing. Possession of another user's Envelope ID or digest grants
nothing. A reviewed package may request authority but cannot create it.

The deliberate trust consequence is that a person who can land source on the authorized
Git ref can request any Task authority inside the caller's server-bound Envelope. That
matches the normal GHA repository model. A customer that does not accept that consequence
must bind the caller to a narrower Envelope/source root or require optional catalog
promotion; Steward must not pretend that a commit digest proves human review.

## Author and operator experience

The DevInt or development team owns a versioned package containing one TaskDefinition,
its prompt, logical Agent, instruction-only skills and assets, execution requirements,
authority requirements, and a complete dependency lock. Conceptually, the GHA caller
selects only:

```yaml
package-path: catalog/release-integration-review/v2/task-definition.json
envelope-ref: <operator-bound Envelope UUID>
envelope-digest: sha256:<expected approved Envelope digest>
```

A directory name such as `v2` is an authoring convention, not the execution identity.
The immutable identity is the ratified repository ID, exact commit, normalized package
path, and Steward-computed closure digest.

The exact repository ID and commit SHA come from ratified GitHub/Identity claims, not
from editable task metadata. The trusted reusable `steward-run` workflow checks out
that exact commit and captures the package itself. Caller-authored preparation jobs
may test the package, but their artifacts are not authoritative package bytes.

The deployment operator configures the allowed caller/source/Envelope boundary,
logical-agent execution bindings, provider capabilities, and the user's issued
Envelope. They do not publish every package version. Ordinary repository variables are
not an authorization boundary: the server-side caller mapping is authoritative and a
transport field can only select or confirm an Envelope allowed by that mapping. GitHub
repository controls remain responsible for the customer's review-and-merge policy;
Steward does not query pull-request approvals.

## Initial supported boundary

The first contract is intentionally narrow:

- transport is GitHub Actions through an exact pinned reusable `steward-run` workflow;
- package source is the same stable repository ID and exact triggering commit ratified
  for the caller;
- source ref must match the Identity policy, initially `refs/heads/main` for the demo;
- package path is relative, normalized, root-contained, and under an operator-allowed
  source root;
- every dependency is a regular file inside that package root and appears exactly once
  in the lock;
- mutable branch names, tags, release names, URLs, symlinks, submodules, executable
  local skills, and cross-repository imports are not package identities;
- one submission creates one disposable Task and no AgentSession or TaskGraph; and
- the Task package cannot select identity, credentials, provider binding, runtime UID,
  image, namespace, native policy, endpoint, CA path, or runner label.

Cross-repository reusable packages can be designed later. They need an explicit source
authorization and retrieval contract and must not be smuggled into this first version.

## End-to-end protocol

### 1. Identity exchange

The governed job exchanges GitHub OIDC through Identity. Identity ratifies stable
repository and owner IDs, workflow path/ref/SHA, triggered commit, event, actor, and the
pinned reusable-workflow identity according to server policy. The caller cannot
override those claims.

The policy maps the caller to a canonical principal, allowed source root, and bounded
set of eligible Envelope identities. Trusting `main` means the customer has chosen
GitHub's repository controls as its source-review boundary. A commit hash proves
immutability, not review; the ratified source policy is what makes that immutable source
admissible.

### 2. Immutable package capture

The trusted reusable workflow checks out the ratified repository at the exact triggered
commit with persisted Git credentials disabled. It builds a deterministic archive from
the selected root without following symlinks or accepting caller-produced package
artifacts, then uploads it using the authenticated principal.

The semantic operation is `createTaskPackageReceipt`. Exact HTTP paths and JSON fields
belong to the contract ticket. The request carries the selected relative path and
package bytes; repository ID and commit SHA are taken from ratified identity, not trusted
from the request.

In this initial design, the pinned reusable workflow is the source-capture trust
boundary: Steward does not separately fetch GitHub to compare the archive with the
commit. Identity therefore admits package upload only from the exact reusable-workflow
identity, and that workflow must perform its own clean exact-SHA checkout. Steward still
independently validates all received bytes and computes the closure. A future server-side
source resolver could strengthen this provenance boundary without changing Task
admission semantics.

Steward enforces size and file-count limits, unique normalized archive paths, strict
JSON parsing, root containment, regular-file types, the complete dependency lock, and
supported schemas. Hard links, symlinks, devices, FIFOs, sparse entries, and archive
metadata that changes or escapes a path are rejected. It computes every content
digest and the canonical closure digest. On success it stores immutable bytes and
returns an opaque principal/source-bound `packageRef` with source identity, digest, and
size. No runtime exists yet.

Identical upload retry under the same idempotency key returns the same receipt. Changed
bytes, path, source, or expected digest under that key conflict. Another principal
cannot discover or consume the receipt.

The receipt is internal protocol, not another developer ceremony. It exists so Steward
can authorize and preserve the exact bytes before runtime creation, retry or re-admit a
parked Task after Git access changes, and prevent a caller from swapping package content
between validation and reservation. `steward-run` performs this step inside the one
governed job.

### 3. Immutable Task input capture

Run-specific inputs remain separate from package instructions. `steward-run` uploads
them through the authenticated Task-input ingress and receives the existing immutable
input receipt. Package dependencies cannot be replaced by Task input, and input files
cannot introduce new execution or authority requirements.

### 4. Direct Task submission

The trusted transport submits a new-version request containing:

- `packageRef` and its expected closure digest;
- the server-issued `envelopeRef` and expected Envelope digest;
- the Task-input receipt;
- ratified trigger correlation; and
- a submitter-scoped Task idempotency key.

The author does not submit an Envelope body or its owner, a principal, an agent command,
runtime details, credentials, or resolved authority. The Envelope digest is a drift
guard, not authorization: Steward still resolves the reference within both the
authenticated principal's issued set and the caller's server-side eligible set, then
requires the exact Envelope to be active and unrevoked.

### 5. Resolution and admission

Before reserving a Task, Steward:

1. authenticates the principal and cross-checks trigger fields against Identity;
2. consumes the package and input receipts for that same principal/source;
3. resolves the issued Envelope by opaque ID and verifies its expected digest;
4. loads the already captured immutable package closure;
5. recomputes the absolute runtime candidate from the TaskDefinition and every locked
   dependency;
6. resolves the logical Agent through the deployment-owned execution binding;
7. evaluates requested models, tools, budget, TTL, and resources against the Envelope;
8. evaluates execution capabilities against the native deployment binding;
9. evaluates credential readiness only for admitted tools; and
10. durably reserves exactly one Task through the common Task application service.

No resolution or validation failure creates or activates an AgentRuntime. An
over-envelope request is rejected or parked unchanged according to the versioned Task
contract; it is never narrowed silently and never widens an Envelope.

### 6. Durable execution and cleanup

After reservation, PR #78's controller-owned state machine remains authoritative:
inert runtime creation, exact UID observation, authority activation, execution,
cancellation, terminal outcome, finalization, and confirmation that all Task-owned
projections are absent. Direct-package invocation introduces no alternate controller or
execution path.

Every activation and execution-start transaction revalidates the exact captured package
resolution, Envelope identity/revision/digest, grants, and deployment binding. A parked
or retried Task never follows a newer commit, package, or Envelope silently.

### 7. Evidence

Final evidence binds the Task and runtime to:

- ratified caller and Git source identity;
- package path, TaskDefinition digest, dependency entries, and closure digest;
- input and output identities/digests;
- Envelope ID, revision, and digest;
- admitted authority and deployment execution binding; and
- lifecycle, revocation, and finalization observations.

This replaces the mandatory catalog-publication witness in the new contract. It does
not weaken signed evidence requirements. The frozen M1 evidence schema remains valid
only for frozen M1 Tasks; the direct-source contract needs its own compatible evidence
version and independent verification fixtures.

## Persisted model and #78 integration

Steward stores a `TaskPackageReceipt` and immutable package blob separately from mutable
Git state. The receipt records the authenticated owner/source, path, byte digest, parsed
artifact digests, closure digest, size, creation time, and idempotency-key digest.

Task reservation snapshots a `ResolvedTaskDefinition` containing the complete absolute
candidate and source/digest evidence needed for admission, re-admission, recovery, and
final evidence. It never re-fetches `main`, a tag, or another mutable name.

Implementation extends #78 at one seam:

```text
legacy/catalog resolver ----\
                             -> ResolvedTaskPlan -> common Task application service
direct package resolver ----/
```

The existing `name@version` and frozen catalog-backed M1 paths remain available under
their own contract discriminators. Direct source must not be disguised as the existing
`workflow` string. Add migrations; do not edit migrations already applied by #78.

## Security invariants and required negative cases

The implementation is incomplete until real-path tests prove:

- caller-controlled repository ID, commit SHA, principal, or Envelope body is rejected;
- an unauthorized repository, workflow, ref, event, reusable workflow, or source root
  cannot create or consume a package receipt;
- a foreign, missing, expired, revoked, rebound, or digest-mismatched Envelope fails
  before runtime creation;
- mutable refs, path traversal, duplicate normalized archive paths, links, submodules,
  devices, FIFOs, sparse entries, archive metadata path overrides, duplicate JSON keys,
  invalid UTF-8, BOM where forbidden, missing/extra lock entries, and changed dependency
  bytes fail closed;
- package/input receipt substitution across principals, repositories, Tasks, or retries
  fails without existence disclosure;
- reuse of an idempotency key with changed package, input, Envelope, or trigger conflicts;
- a package cannot inject credentials, bindings, runtime identity, image, native policy,
  namespace, endpoints, or transport configuration;
- an over-envelope package creates no effectful runtime authority before approval;
- approval, Envelope, package, or deployment-binding drift is revalidated before
  activation and does not strand a Task in a recreation loop;
- queued and running Tasks remain bound to their captured source when `main` advances;
  and
- success, failure, cancellation, rejection, expiry, and recovery all preserve exact
  runtime-UID cleanup and no-silent-replay guarantees.

## Compatibility and rollout

- Do not edit or reinterpret `steward.m1/v1`; introduce a new explicit contract version.
- Preserve the current legacy `name@version` caller for the prepared demo until the
  direct path passes its own E2E and a deliberate caller migration is approved.
- Release Steward package ingress/resolution and `steward-run` transport as compatible
  pinned revisions. A partial rollout must fail closed as unsupported contract.
- Stage API writers before activating the new caller, using #78's existing staged
  orchestration controls where applicable.
- GitOps owns retained local-main/stable deployment and readiness. Product development
  and conformance use isolated, run-owned environments.
- Rollback selects the old caller contract; it never resolves a direct Task to mutable
  source or rewrites an already reserved Task.

## Alternatives considered

- **Trust a package artifact prepared by caller code:** rejected. It lets an earlier
  untrusted job substitute bytes while presenting the same source metadata.
- **Have Steward fetch every package from GitHub:** stronger independent Git provenance,
  but it adds a server-side repository credential and provider availability to every
  admission. Keep it as a future hardening option; the first path trusts the exact
  pinned reusable workflow to perform a clean checkout.
- **Use only a caller-supplied package digest:** rejected. A digest provides integrity
  but no repository, review, principal, or Envelope authorization.
- **Keep catalog publication mandatory:** retained as the frozen M1 and optional curated
  distribution path, but rejected as the default same-repository developer experience.
- **Inline prompt or TaskDefinition in the Task request:** rejected. It bypasses the
  ratified source and complete dependency boundary.

## Implementation plan and proposed ticket split

No implementation ticket should start changing wire contracts before this architecture
and the cross-product field ownership are approved.

| Proposed ticket | Primary owner | Deliverable | Depends on |
|---|---|---|---|
| DP-C01: direct-source contracts | Steward with Identity, steward-run, agentic-ops review | Versioned package receipt, Task submission/status/evidence schemas and positive/negative fixtures | Architecture approval |
| DP-A01: portable package conformance | agentic-ops | Canonical package layout, strict validator, closure builder, migration of the demo package without runtime authority fields | DP-C01 package fixtures |
| DP-R01: trusted GHA transport | steward-run | Exact-source checkout, package upload, input upload, direct Task submission, polling/output/finalization, interruption cancellation | DP-C01 transport fixtures |
| DP-I01: ratified source authorization | Identity and Steward | Exact caller/source-root claims and cross-checks; no caller-authored stable IDs | DP-C01 identity fixtures |
| DP-S01: package ingress and resolver | Steward | Immutable receipt/blob storage, validation, closure computation, authorization, idempotency, and additive migrations | DP-C01; can develop beside DP-A01/R01/I01 |
| DP-S02: Task/evidence integration | Steward | `ResolvedTaskPlan` convergence into #78, revalidation, status and signed direct-source evidence | DP-S01 and DP-C01 evidence fixtures |
| DP-E01: direct GHA conformance | Cross-product; GitOps owns retained lanes | Positive, authority-negative, substitution, recovery, evidence, and cleanup E2E through normal GHA | DP-A01/R01/I01/S02 compatible candidates |

After DP-C01 freezes fixtures, DP-A01, DP-R01, DP-I01, and most of DP-S01 are safe to
develop in parallel in separate repositories or worktrees. DP-S02 has one Steward code
owner and begins after the resolver/store boundary is stable. Heavy integration lanes
remain serialized. DP-E01 is the integration gate, not a place to invent missing
contracts.

## Acceptance criteria

The direct path is complete when:

1. a reviewed `agentic-ops` commit invokes its own TaskDefinition without a catalog
   publication or administrator copy step;
2. the trusted transport and Steward agree on the exact ratified source and Steward
   independently validates and persists the complete closure;
3. only an active Envelope issued to the authenticated principal can authorize the
   recomputed candidate, with exact digest drift detection;
4. all negative cases above fail before unauthorized execution;
5. two normal GHA invocations return useful output and independently verifiable source,
   model/tool, lifecycle, and cleanup evidence; and
6. legacy and frozen M1 compatibility lanes remain green and contract-separated.

## Explicit non-goals

- mandatory catalog publication, catalog discovery, or GitHub release/tag semantics;
- cross-repository package imports;
- making a package digest or Envelope digest an authorization token;
- Steward enforcement of GitHub pull-request review rules;
- arbitrary executable skills supplied by the package;
- AgentSession, AgentInstance, TaskGraph, DEV readiness, or multiple-Task workflows; and
- weakening #78's admission, approval, revocation, execution, or cleanup lifecycle.

## Approval questions

Architecture approval should explicitly confirm:

1. reviewed source on an Identity-authorized Git ref is sufficient source approval;
2. the initial path is limited to packages in the invoking repository at the exact
   triggered commit;
3. the GHA package path plus ratified source identity is the human-facing reference,
   while Steward computes and records the closure digest;
4. `envelopeRef` plus expected Envelope digest is protected transport configuration,
   never portable package content, and server-side caller-to-Envelope binding remains
   authoritative;
5. trusting the exact pinned reusable workflow's clean checkout is sufficient for the
   first source-capture boundary, with server-side Git retrieval deferred;
6. catalog publication becomes optional and the frozen `steward.m1/v1` remains
   unchanged; and
7. the proposed ticket split and parallelization boundary are acceptable.
