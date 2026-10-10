# Steward administrator browser contract v1

Status: active browser and API boundary.

Applies to Steward 0.3.15.
Sections that describe unreleased behavior say so explicitly.

## Presentation ownership

The Next.js application under `web/` is Steward's only browser presentation
surface. The apiserver serves JSON APIs and authentication protocol endpoints;
it does not embed or serve HTML, CSS, JavaScript, or a second administrator
dashboard.

The browser routes `/admin/*`, `/connections`, `/envelopes`, `/runs`, and
`/settings` are owned by Next.js. The compatibility route
`/admin/connections` redirects to `/connections`.

## Authentication boundaries

Browser administrator APIs use the opaque browser session and local RBAC
authority described in `browser-session-contract-v1.md`. Missing browser
authentication returns `401`; an authenticated ordinary user returns `403`.
Only a session resolved with the administrator role receives
`BrowserAdminAuthority`.

The separate operator API uses `RequestAuthenticator`. It first performs
Kubernetes `TokenReview`; when that rejects invalid credentials, a configured
verified Identity Task JWT may authenticate through the existing fallback.
Either path must yield the configured exact administrator group. A member role,
Task, runtime, provider identity, browser cookie, or route-scoped steward-run
bootstrap identity without that group is not operator authority.

Steward does not accept a provider token in a URL, HTML document, Web Storage,
cookie, or JavaScript configuration. Browser sessions cannot inject a bearer
assertion into the operator boundary, and Kubernetes bearer credentials cannot
satisfy the browser-session boundary.

## Versioned browser API

The browser API prefix is `/admin/api/v1`. Its media type is JSON. Rust
request and response types are included in Steward's OpenAPI document, which
is the typed source of truth. The browser derives navigation from its current
Next.js route map and session response; there is no separate UI-bootstrap API.

Browser API responses set `Cache-Control: no-store`, a self-only Content
Security Policy, `Referrer-Policy: no-referrer`, clickjacking protection,
content-sniffing protection, and a restrictive Permissions Policy.

Every authenticated browser mutation requires:

- the exact configured `Origin`;
- `Sec-Fetch-Site: same-origin`;
- JSON content type;
- `X-Steward-CSRF` equal to the current server-side session value.

These checks are additive and never establish identity. Steward emits no CORS
opt-in.

## Administrator surfaces

### Approvals

The Next.js `/admin/approvals` page consumes the unified, cursor-paginated
`GET /admin/api/v1/requests` read model, its exact-ID detail route, and
`GET /admin/api/v1/requests/summary`. The queue combines Envelope requests,
runtime exceptions, and cumulative spend or runtime-minute escalations without flattening their
source-specific decision routes. Structured `DirectAdmissionDelta` values are
the only source for rendering requested changes; the browser must not parse the
legacy runtime-exception `counterexample` string.

Automatic Envelope provisioning is recorded with `system:auto` as the status
actor. Manual approvals retain rationale, evidence URL, expiry, and the exact
canonical administrator actor. Filing an external decision first acquires a
short-lived per-request lease, so concurrent browser retries cannot create two
external decisions. The completed reference and status history are append-only.
The approval route continues to accept the legacy empty object only for an
unfiled within-ceiling request with no new decision metadata. A rationale is
mandatory for every ceiling-exceeded approval, whenever evidence or expiry
metadata is supplied, and whenever an external decision has been filed.

### Envelopes

The Next.js envelope administration pages consume versioned JSON template and
request APIs. Templates have immutable IDs, administrator-authored display
names, one or more eligible member roles, and append-only revisions. More than
one active template may target the same role; an Envelope request pins the
chosen template ID and revision. A custom request instead omits both template
fields, carries the complete requested Envelope, and always requires an explicit
administrator decision. It never invents a template or member role.
Custom requests and approvals return `422` unless the requested authority is
within the deployment's current `customEnvelopeSafetyCeiling` and every model
and tool remains available in the capability catalog.

`POST /admin/api/v1/envelopes/provision` lets an authenticated administrator
provision an exact template revision for an existing canonical user. Steward
checks the target's current member-role eligibility and the requested authority
against that revision's ceiling. The target remains the immutable owner and the
administrator is the event actor.

Users may retain active Envelopes from different templates when their content
digests differ. Replacement stales only the prior active Envelope from the same
template; the same digest under a different template is a conflict. See
[`operator-envelope-administration.md`](operator-envelope-administration.md)
for the complete transaction and task-selector behavior.

Template responses retain `memberRole`, set to the first eligible role, as a
compatibility alias. Catalog-aware clients use the authoritative `memberRoles`
array.

#### Unreleased: capability tool picker

The template editor presents the deployment capability catalog as a searchable
tool picker. Optional `toolsets` on a catalog tool are authoritative,
multi-membership presentation metadata; the UI never derives a group from a
provider, resource, or action name. Tools without metadata remain selectable in
a deterministic fallback group. Bulk actions may select read-only tools only.
Write and destructive grants require an explicit per-tool confirmation, and a
new revision displays its exact added and removed tool tuples before saving.
The saved Envelope contains only the deduplicated `(provider, resource, action)`
tuples, never the presentation metadata.

Provisioned Envelope requests may include current-period spend usage. Usage is
the sum of the latest observation for each runtime bound to that Envelope
instance, plus active instance-scoped top-up grants in the effective limit. An
`available`, `partial`, or `unavailable` status is authoritative; presentation
must never guess a missing value. Request detail includes append-only status
history.

An Envelope revision may also constrain cumulative runtime minutes for each
provisioned instance. This authority is stored in the immutable Envelope snapshot,
not added to the AgentRuntime CRD. Steward derives current-period usage from
append-only Task running-to-terminal lifecycle intervals, records observations and
exhaustions append-only, and suspends execution when the effective limit is
exhausted. Administrator top-ups are instance-scoped append-only grants; a
successful grant must raise the effective limit above the recorded usage before
the controller can resume the parked Task. Denial cancels the parked Task.

Capability metadata supplies tool access class and provider catalog
availability. The browser must not infer either from display text. Unsupported
models are omitted rather than rendered as guessed disabled options. Admission
remains the authority for exact deltas and Envelope revisions.

### Connections and onboarding

The administrator setup guide described below is unreleased.

The administrator workspace exposes `/admin/get-started`. Its source of truth
is the administrator-only, read-only `GET /admin/api/v1/setup-status` route.
The response derives current checks from deployment configuration and live
Steward records: active orchestration and resolvable execution bindings, the
signed-in administrator's latest successful GitHub Connect start and duration,
published capability count, member-ready templates, other active canonical
users, configured task-identity discovery, whether direct-package use makes
`githubSource` required, unassociated actors from the configured GitHub
task-identity issuer, and bounded owner-scoped evidence from Tasks carrying
durable GitHub-ratified source provenance. It never returns provider tokens,
OAuth continuations, raw task failure text, or credentials.

The UI refreshes the endpoint 15 seconds after the preceding request settles
and on explicit request, so a slow authoritative read is not repeatedly
aborted by overlapping polls. Each refresh enters an explicit loading state; a
failed refresh discards prior cards and renders unavailable rather than
retaining a stale ready result. Polling stops and its active request is aborted
when the guide is hidden or unmounted. Hiding the administrator guide is
deliberately a presentation-only preference stored under the exact
browser-local key `steward.ui.admin-setup-dismissed`; it changes no server state
or authority and can be restored from administrator settings. This differs from
user onboarding, whose progress and dismissal are durable server preferences.

Capability-catalog v2 reports published tools but has no expected-tool count or
authoritative gateway-publication provenance. Until that metadata exists, the
setup endpoint reports this check as `unknown` rather than inferring provenance
from names or counts. Likewise, Steward can report that task-identity discovery
is configured and whether recent identities are associated, but it cannot prove
an installation-specific repository policy without a concrete repository
submission. The check links to installation guidance and retains only the
latest bounded error from an owner-scoped Task with durable GitHub-ratified
source provenance. GitHub Actions automation is ready only after at least one
such Task has durably succeeded; unrelated Tasks are not evidence. Versioned
workflow reservations persist the authenticated provenance independently of
direct-package binding evidence, so versioned-workflow-only installations can
become ready without the direct-package `githubSource` adapter. Historical
versioned Tasks without that evidence remain unknown rather than being inferred
from submitter names. When no ratified Task exists and no bounded ratified
failure is recorded, the status is `unknown`: authentication or policy failures
before Task reservation leave no AgentRun record and therefore cannot truthfully
be reported as ready.

`GET /app/api/v1/connections` returns connected providers and the available
provider catalog. GitHub is enabled; unavailable providers remain explicit and
disabled. Provider `start` and `disconnect` mutations are browser-session and
CSRF scoped. A successful `POST /app/api/v1/connections/{provider}/start`
returns HTTP 202 with an opaque operation identifier, the server-owned
`pollDeadlineAt`, and no provider URL. The browser polls
`POST /app/api/v1/connections/{provider}/disconnect` likewise returns HTTP 202
with an operation identifier and polling deadline. The browser polls
`GET /app/api/v1/connections/{provider}/operations/{operation_id}`: pending
operations return HTTP 202, while a completed operation returns HTTP 200 with
the one-time authorization URL, successful disconnect, or a bounded terminal
failure. Operation reads are exact-owner scoped and use HTTP 404 for absent,
foreign, provider-mismatched, or unsupported operation identifiers. An active
operation advertises its bounded runtime-response deadline; a reused,
succeeded start whose OAuth flow is still pending advertises the later flow
expiry so a retry can retrieve the still-valid URL. The browser aborts the poll
when its view is replaced or unmounted. It observes a short, locally bounded
grace after the advertised execution deadline so Steward can publish the
durable deadline result, and always permits an immediate read of a reused
terminal operation. An unobserved deadline leaves an actionable retry state. It
never persists the authorization URL. The Connections UI renders distinct
recovery guidance for runtime authentication, token grant, provider-response,
gateway transport/status/body/unavailable, runtime creation, runtime start, and
deadline failures. These bounded categories come from the durable operation
record; arbitrary runtime diagnostics remain out of the browser response. The
onboarding aggregate composes connection, Envelope, workflow,
and run evidence; dismissal and the explicit "I added the workflow"
acknowledgement are server-side preferences. When browser surfaces are enabled
and at least one execution binding is advertised, Steward publishes the
reserved immutable `repo-summary@2` sample against the first binding in
lexical order. Revision 2 instructs the agent to write its Markdown result to
`out/summary.md`; only files beneath `out/` are returned as governed Workflow
output. Existing installations retain immutable revision 1 and append revision
2 during startup. An existing reserved revision must match the complete
system-authored identity and digest or startup fails closed. Administrator
publication cannot use the reserved name. Removing revision 2's pinned
execution binding hides the sample without preventing browser startup. The renderer
returns a deterministic suggested path, but callers may use any valid GitHub
workflow filename. Steps four and five follow all result pages and complete only
after a GitHub-triggered run pins that sample revision and one of the user's
provisioned Envelope instances. Steward does not dispatch the run; it is
launched from GitHub with `gh workflow run` or the Actions UI.

The administrator Workflow form states the same output contract. A published
Workflow prompt must tell its agent to create every result file beneath `out/`;
stdout and the agent's final message do not implicitly become output files.

### Fleet and runs

PostgreSQL Task state is the browser run source of truth. User routes are
exact-owner scoped; administrator `all-runs` routes use browser administrator
authority and may expose the owner's display email. List responses include
phase facets computed from the current filter with the phase predicate removed.

Run detail exposes validated GitHub source provenance when it was captured at
submission, four bounded stages (admission, runtime provisioning, agent
execution, and finalization), and one execution step with stdout/stderr streams.
Stage IDs and timeline stage-event payloads are closed enums; admitted events
carry the pinned Envelope revision/digest, runtime-bound events carry the
runtime UID/ownership, and execution-ended events carry a typed terminal exit
category.
The separate log endpoint supports bounded byte-offset reads while a Task is
running and marks whether the stream is complete. Cancel is owner scoped and
returns `409` after a run is terminal. Administrator run detail is read-only and
does not render cancel or re-run controls.
Re-run creates a fresh Task for a versioned Steward workflow. For a direct
GitHub Task, Steward invokes only the governed MCP-GW
`actions_run_trigger.rerun_workflow_run` operation through the caller's GitHub
connection, then correlates the new Task by exact repository and GitHub run ID
with a higher run attempt. A `202` response is polled with the same browser
idempotency key until that Task exists. Correlation does not depend on the
suggested or actual workflow filename and never copies old provenance.

### Federated Task subjects

The browser administrator API exposes read-only list/detail/audit operations at
`/admin/api/v1/federated-subjects` and revision-checked `associate`, `replace`,
and `disable` mutations below each subject ID. These APIs manage the exact
verified issuer/subject association only. They do not create a canonical user,
approve a User Envelope, or authorize a Task. Mutation actor identity always
comes from `BrowserAdminAuthority`; request bodies cannot name the actor.

Subject list and detail responses expose `associationMethod` when a proof method
is recorded: `admin`, `connection-verification`, or `v2-claim`. Audit responses for
`connection_verified` expose `connectionProvider=github` and the immutable
numeric `connectionAccountId`; those evidence fields are absent for every other
action. Login, display name, and email remain display metadata rather than
association evidence. The default GitHub Connect path may create the
connection-verified association before any v3 Task observation. It never
replaces a conflicting association or re-enables a disabled subject.
For such a subject, `firstSeenAt` originates from the verified Connect event and
the initial `lastSeenAt` is equal to it; only a later signed Task observation
advances `lastSeenAt` or its display metadata. Re-reading connection status is
side-effect free. Disconnecting or later connecting another verified GitHub
account does not remove an existing association; explicit administrator
disable remains the revocation path.

An administrator should first inspect the observation and intended canonical
user, then submit `expectedRevision` and `canonicalUserId`. A `409` requires a
fresh read and review, not an automatic retry. Disabling may retain the former
canonical-user reference for audit while immediately preventing resolution.

## Deployment boundary

The chart routes browser API and authentication prefixes to the apiserver and
all presentation routes to the Next.js web service. The apiserver and web
service remain separately deployable, but there is one browser presentation
implementation.
