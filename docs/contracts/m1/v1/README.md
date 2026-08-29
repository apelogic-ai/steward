# Cross-product M1 contract v1

Status: frozen for implementation and cross-product review
Contract identifier: **steward.m1/v1**
Targets: Steward v0.2.0, steward-run v0.5.0,
Identity/github-oidc-exchange v0.4.0
Schema dialect: JSON Schema Draft 2020-12

This directory is the sole authority for M1 public field names, values, ownership,
compatibility, and authority decisions. It defines contracts only; downstream tickets
implement them.

Normative artifacts:

- [manifest.json](manifest.json) indexes every public definition and fixture;
- [m1-contracts.schema.json](schemas/m1-contracts.schema.json) is the closed schema
  bundle; and
- [fixtures](fixtures) contains accepted and rejected consumer examples.

All JSON objects are closed. Unknown fields are rejected. This document is normative
for ordering, ownership, digest, idempotency, timing, and verification behavior that
JSON Schema cannot express.

## Version and compatibility selection

An M1 request contains contractVersion equal to steward.m1/v1. That exact field selects
this contract; product version, URL, workflow name, Identity token, and User-Agent do
not select it.

A request without contractVersion is not M1. During Steward v0.2.x only, the legacy
adapter may accept the exact legacyTaskSubmission shape from steward-run v0.4.0 at
immutable workflow commit `bab2df96b29d1abe4db0a2c3fc121956b5b2dbe6` and action
commit `19230cc59a6b1246224912961e35c7044b0808d3`:

- legacy workflow is unqualified name@version;
- M1 workflow is qualified catalog/name@version;
- legacy may contain the optional diagnostic agentRuntimeUid from that pinned client;
- M1 rejects both and requires envelopeRef, trigger, and input; and
- an M1 Identity token is never routed through the legacy adapter.

The adapter has exactly one platform-configured migration catalog and maps the legacy
name into it. It fails when mapping is absent or ambiguous and never searches catalogs.
The pinned client keeps its prior six-operation semantics:

1. POST /v1/tasks with legacyTaskSubmission returns 201 or 202;
2. PUT /v1/tasks/{taskUid}/inputs with application/x-tar returns 204;
3. POST /v1/tasks/{taskUid}/execute returns 202;
4. GET /v1/tasks/{taskUid} returns 200 while polling;
5. GET /v1/tasks/{taskUid}/outputs returns 200 application/x-tar; and
6. DELETE /v1/tasks/{taskUid} returns 202 to finalize.

JSON responses retain the closed pre-M1 Task response fields taskUid, nullable
runtimeUid, phase, runtimeOwnership, finalized, optional failureReason, and deltas. The
adapter adds no M1 response field because steward-run v0.4.0 rejects unknown fields.
Steward classifies the resulting journal evidence as legacy_unsigned; it exposes no
signed M1 evidence for that Task and never back-signs it.

The adapter is removed in Steward v0.3.0. New integrations use steward-run v0.5.0. The
[rejected M1 unqualified request](fixtures/compatibility/m1-unqualified-workflow.json)
and [accepted legacy request](fixtures/compatibility/legacy-task-submission.json) fix
this boundary.

## Common representations

workflow is exactly:

~~~text
catalog/name@positive-decimal-version
~~~

Catalog and name are lowercase DNS-label-like slugs of at most 63 bytes. Version has no
sign, leading zero, prerelease, range, tag, branch, digest, source URL, or path.

envelopeRef and immutable Steward object IDs are lowercase RFC 4122 UUID text accepted
by the schema. Values such as input:abcdefghijkl are opaque non-secret references. The
prefix names a type; the suffix has no client meaning. Every dereference still requires
the authenticated principal.

All digest fields are lowercase SHA-256 identifiers. M1 manifests are UTF-8 JSON.
Duplicate keys, invalid UTF-8, non-finite numbers, and byte-order marks are rejected.
Manifest content digests cover RFC 8785 canonical JSON. Markdown and other declared
assets are digested as exact Git blob bytes without newline or Unicode normalization.

The dependency closure is the RFC 8785 canonical JSON array of all lock entries, sorted
bytewise by kind and then ref. closureDigest covers that array. Every manifest, prompt
body, instruction file, asset, and agent record has exactly one entry. The lock does not
lock itself. Undeclared files, duplicate refs, cycles, changed bytes, missing or extra
entries, and paths outside the publication root fail.

## Authenticated input and one-Task lifecycle

All operations require the same authenticated bearer principal and a submitter-scoped
Idempotency-Key. Keys are opaque 16–128 byte values, never returned or logged. Only
their SHA-256 digest may be journaled.

steward-run v0.5.0 uploads the triggered-revision tar to POST /v1/task-inputs with
Content-Type application/x-tar. Steward enforces the 64 MiB limit, computes digest and
size, stores immutable bytes, binds them to the authenticated principal and ratified
source, and returns 201 taskInputReceipt. Same-key, same-byte retry returns 200 and the
same receipt. Same key with different bytes returns 409 input_idempotency_conflict.
Another principal receives 404 for the ref. The caller cannot provide owner, source,
digest, size, storage URL, or credentials.

POST /v1/tasks accepts taskSubmission. Steward checks, in order:

1. authenticate and validate the exact Identity contract;
2. validate the closed request;
3. cross-check trigger fields against ratified Identity source;
4. find envelopeRef by UUID equality inside the authenticated principal's issued set;
5. only then resolve optional team context and member role;
6. consume the input receipt for the same principal and source;
7. authorize and resolve catalog coordinate and dependency closure;
8. recompute absolute typed admission and credential readiness; and
9. create one Task.

A foreign Envelope UUID returns 404 envelope_not_authorized whether or not it exists.
Missing, revoked, expired, rebound, or malformed Envelopes fail before runtime creation.
teamRef is required only for team-scoped human work and omitted for M1 service work.
Principal, acting user, member role, Envelope body/revision/digest, runtime UID, model,
tools, credential mode, runner, image, and native policy are never request fields.

A new Task returns 202 taskCreateResponse. Exact submission retry returns 200 and the
same Task. Same key with any changed immutable field returns
409 task_idempotency_conflict.

The phase graph is:

~~~text
submitted --> parked --> queued --> running --> succeeded
     |           |          |          +------> failed
     |           +----------+-----------------> cancelled
     +----------------------------------------> cancelled
~~~

parked moves to queued only when the original absolute candidate is admitted unchanged.
Terminal phase never changes. A deliberate rerun creates a new Task UID. There is no
public Run or attempt entity.

runtimeUid is nullable diagnostic correlation, including in final evidence when no runtime
was created. It cannot select, own, retry, authorize, or address a Task. GET
/v1/tasks/{taskUid} returns taskStatus. failureCode is the only
generic failure detail. GET /v1/tasks/{taskUid}/outputs returns authenticated tar only
when result state is available; result ref is evidence identity, not a download token.

Finalization is orthogonal to phase. POST /v1/tasks/{taskUid}/finalize accepts
taskFinalizationRequest. complete requires the observed terminal expectedPhase; cancel
requests cancellation from any non-finalized state. It returns 202 while enforcing or
200 when finalized. Finalized means Task runtime, model, tool, network, and credential
projections are torn down and final evidence is available. Transport interruption uses
cancel and never abandons a Task.

## Identity claims

Identity owns and signs identityClaims. Callers cannot request or override a claim.
repositoryId, repositoryOwnerId, and actorId are stable provider IDs. workflowPath,
workflowRef, and workflowSha are ratified provenance: path is repository-relative, ref is
the triggering Git ref, and SHA is the immutable workflow revision. Mutable repository,
owner, actor, branch, or organization display names never authorize.

catalog_scopes is a server-policy upper bound:

~~~text
Identity catalog_scopes
  intersect Steward tenant/source/catalog bindings
  intersect permitted cross-catalog imports
~~~

Steward may narrow or reject and cannot authorize an absent catalog. Identity naming a
scope cannot force acceptance.

principal.kind is independently resolved as service or human_derived; trigger transport
does not select it. Service has an opaque subject and named canonical ownerUserId.
Human-derived additionally has actingUserId. Organization-wide service subjects and
caller-selected owners are forbidden.

## Agentic catalog artifacts

Public artifacts are taskDefinition, prompt, localSkill, dependencyLock, agent,
executionRequirements, and authorityRequirements. M1 uses JSON manifests, exact
Markdown content, one instruction-only local skill, one complete lock, one logical
versioned agent, execution capabilities shell and python3, and typed model/tool/USD
budget/TTL/runner-resource requirements.

Artifact authors cannot select Envelope, credential mode/binding, connection, image,
runner label, RuntimeClass, OpenShell policy/profile, endpoint, audience, CA path,
secret, source repository, or Task principal. Instruction-only skills cannot contain
scripts.

Steward recomputes one absolute candidate from TaskDefinition and all dependencies on
every admission and re-admission. Model, tool, budget, TTL, and resources are checked
against the immutable Envelope. Execution requirements are separately checked against
the GitOps-owned native OpenShell binding. No check widens another; excessive requests
are rejected or parked unchanged.

## Create-only catalog publication

catalogPublicationRequest plus Idempotency-Key binds qualified coordinate to stable
repository ID, exact 40-hex commit, root-contained path, content digest, and closure
digest. First create returns 201 catalogPublicationWitness. Git remains authored source;
the witness is append-only.

| Existing state | Request | Result |
|---|---|---|
| no coordinate/key | first create | 201, one witness |
| same key and canonical request | retry | 200, same witness |
| same key, any changed field | conflict | 409 idempotency_conflict |
| new key, same coordinate, even identical | republish | 409 coordinate_already_published |
| coordinate with changed source/digest | rebinding | 409 coordinate_rebinding_rejected |

Failed verification creates no witness. Deleted/rewritten source or changed bytes after
publication are integrity failures, not publication opportunities. URLs, mutable refs,
provider credentials, and unapproved paths never enter the witness.

## Tool capability and credential readiness

The Steward-governed registry publishes one toolCapability per canonical capability.
provider is metadata only. Exact pairs are:

| credentialMode | bindingKind |
|---|---|
| none | none |
| user_connection | principal |
| workload_identity | workload |
| server_managed | server |

Provider never implies mode. GitHub provider does not imply user_connection, and
github_actions origin satisfies no GitHub tool binding.

After authority admission, Steward checks only actually admitted tools. Unused Envelope
capacity creates no requirement. Empty and none sets perform no provider lookup. A user
connection cannot satisfy workload identity.

bindingRef is non-secret identity. Status is ready, missing, mismatched, unavailable, or
revoked. Failures produce structured credential_binding blockers without provider error
strings or credential values. toolBindingStatus fixes the allowed combinations: mode none
has no bindingRef and is ready/not_required; credentialed modes have a bindingRef and are
either ready/satisfied, missing-or-mismatched-or-unavailable/blocked, or revoked/revoked.

Envelope issuance evaluates authority only and never live connection readiness.
Readiness is evaluated for a Task after Envelope selection and authority admission.

## Signed evidence and independent verification

Every finalized M1 Task produces taskEvidencePayload binding Task/runtime, trigger,
principal, publication and all dependency digests, input/output, Envelope
UUID/revision/digest, native OpenShell evidence, admitted authority, non-secret binding
evidence, admission, revocation, and lifecycle timestamps.

Cancellation before admission or runtime creation is explicit rather than omitted:
runtimeUid is null, executionBinding is not_created, admittedAuthority is not_admitted,
output is absent, and unavailable transition timestamps are null. The
cancelled-before-runtime fixture fixes that terminal shape. M1 taskStatus evidence is pending
until finalization and available afterwards. legacy_unsigned belongs only to the isolated
pre-M1 adapter response, not to an M1 taskStatus.

Evidence excludes tokens, keys, OAuth values, provider credentials, prompt content, raw
source credentials, secret refs, and unbounded errors.

Signing:

1. validate taskEvidencePayload;
2. serialize RFC 8785 canonical UTF-8 JSON;
3. set payload type to application/vnd.steward.task-evidence.v1+json;
4. sign DSSE v1 pre-authentication encoding;
5. emit exactly one Ed25519 signature; and
6. publish only the dedicated attestation public JWK.

DSSE pre-authentication encoding is the ASCII word DSSEv1, a space, unsigned decimal
type byte length, a space, type bytes, a space, unsigned decimal payload byte length, a
space, then payload bytes. payload and sig are unpadded base64url. Signature key ID
matches payload issuer.keyId and exactly one published JWK kid. Runtime mint identity is
a different purpose and trust root.

An independent verifier obtains material through a separately trusted channel, pins
instanceId, rejects wrong/duplicate key IDs, algorithms, payload type, or signature
count, verifies Ed25519 over exact DSSE bytes, requires payload bytes already canonical,
validates taskEvidencePayload, matches expected Task/runtime/issuer, recomputes all
available digests, and checks lifecycle/revocation timing.

Schema validity is not signature validity. The positive payload, signed evidence, and
verification-material fixtures form one cryptographically valid deterministic vector. Its
canonical payload is 4,418 bytes with SHA-256
sha256:37393b8d4ee03b51f4dbd130f6b56c3e865bdc9b61a1efb0433f1cb3c42028dd.
Its DSSE PAE is 4,479 bytes with SHA-256
sha256:97380a148e8af8a40aa5317e5c4a87f4a239c75eadce0fed2bd77f1fdad22fcd.
The public key is the RFC 8032 Ed25519 test key; no private material is published. An
independent implementation must decode the signed fixture payload to the exact RFC 8785
serialization of task-evidence-payload.json and verify its signature with
evidence-verification-material.json. M1-S06 adds deployed-signer and tamper vectors.
legacy_unsigned never passes this verifier.

## Revocation and TTL

M1 revocation propagation target is **60 seconds**. Maximum authority-free sandbox drain
is **300 seconds**.

- requestedAt: durable revocation acceptance for exact immutable Envelope;
- observedAt: controller first observes it;
- deadlineAt: requestedAt plus 60 seconds;
- authorityDisabledAt: latest successful disable acknowledgement across Task model,
  tool, credential, and external-network projections;
- drainStartedAt: process drain start, not an authority event;
- drainDeadlineAt: authorityDisabledAt plus 300 seconds;
- teardownStartedAt/teardownCompletedAt: runtime destruction; and
- finalizationRequestedAt/finalizedAt: durable Task closure.

Times are UTC RFC 3339; E2E uses monotonic measurement correlated to them. enforced
requires authorityDisabledAt no later than deadlineAt. Otherwise state is
deadline_missed and conformance stays red.

An operation is in flight only if its enforcing gateway accepted it before
authorityDisabledAt. It may complete within the gateway's bounded timeout. Retry,
redirect needing authorization, new stream/connection, refresh, or follow-up is a new
action and must fail after authorityDisabledAt. A draining process holds no model, tool,
credential, or external-network authority and is forcibly torn down by drainDeadlineAt.

TTL is a separate scheduled full re-admission clock. Revocation never waits for TTL.
Process uptime, in-flight work, gateway failure, and drain extend neither clock.

## Field ownership

| Field/artifact | Writer | Validator/consumer |
|---|---|---|
| workflow, protected envelopeRef, optional teamRef | steward-run transport | Steward |
| GitHub trigger fields | steward-run from GitHub context | Steward cross-checks Identity |
| manual trigger fields | authenticated manual adapter | Steward session boundary |
| input bytes | transport | Steward input ingress |
| input receipt fields | Steward | Task submission |
| stable source IDs/provenance/scopes | Identity server policy | Steward narrows/authorizes |
| principal kind/subject/owner/actor | Identity plus Steward mapping | never request-owned |
| TaskDefinition/prompt/skill/lock/requirements | agentic-ops | Steward resolver |
| publication binding/witness | Steward publisher | journal/evidence |
| Envelope UUID/revision/digest | Steward Envelope store | admission pins |
| member role | Steward team resolver | never request-owned |
| capability mode/binding kind | Steward registry selected by GitOps | admission |
| binding identity/status | adapter through Steward port | admission/evidence |
| OpenShell policy/image/capabilities | GitOps/OpenShell | Steward verifies |
| Task state/result/finalization | Steward | Task clients |
| runtime UID | Steward controller | diagnostic only |
| evidence/signature | Steward evidence subsystem | independent verifier |
| revocation timestamps | enforcing components via Steward journal | verifier/conformance |

A schema accepting a field does not transfer ownership to the HTTP caller.

## Validation

Run:

~~~bash
cargo xtask m1-contracts --check
~~~

The normal quality lane requires the exact definition set, supported schema keywords
and patterns, positive and negative fixtures for every definition, and malformed,
unknown-field, privilege-injection, and compatibility coverage. It also decodes the
deterministic payload, compares its canonical bytes, and uses OpenSSL to verify its
Ed25519 DSSE signature against the published key. Consumers may generate types from
definitions, but this schema and document remain authoritative.

## Closed decisions

- JSON manifests plus exact-byte Markdown/assets are the M1 formats.
- One Task has one lifecycle; there is no Run.
- workflow is qualified and envelopeRef mandatory.
- Envelope membership precedes team/role checks and leaks nothing.
- Trigger transport and principal kind are independent.
- Stable IDs authorize; catalog scopes are only an upper bound.
- TaskDefinitions never select Envelope, credentials, runtime, or native policy.
- Publication is create-only, including identical re-publication.
- shell and python3 are the complete M1 portable vocabulary.
- Provider and GHA origin imply no credential mode.
- Envelope issuance is independent of connections.
- Evidence is RFC 8785 plus DSSE v1, one Ed25519 signature, dedicated trust root.
- Revocation is 60 seconds; drain is at most 300 seconds; TTL remains separate.
- Legacy pinned compatibility is isolated, unsigned, and ends at Steward v0.3.0.

Changing one of these is a new versioned contract and breaking-change review.
