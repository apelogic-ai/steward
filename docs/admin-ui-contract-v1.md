# Steward administrator browser contract v1

Status: active browser and API boundary.

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

The separate operator API uses `RequestAuthenticator` and Kubernetes
`TokenReview`. A caller must have the configured exact administrator group.
A member role, Task, runtime, provider identity, browser cookie, or
route-scoped steward-run bootstrap identity is not operator authority.

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
runtime exceptions, and cumulative-spend escalations without flattening their
source-specific decision routes. Structured `DirectAdmissionDelta` values are
the only source for rendering requested changes; the browser must not parse the
legacy runtime-exception `counterexample` string.

Automatic Envelope provisioning is recorded with `system:auto` as the status
actor. Manual approvals retain rationale, evidence URL, expiry, and the exact
canonical administrator actor. Filing an external decision first acquires a
short-lived per-request lease, so concurrent browser retries cannot create two
external decisions. The completed reference and status history are append-only.

### Envelopes

The Next.js envelope administration pages consume versioned JSON template and
request APIs. Templates have immutable IDs, administrator-authored display
names, one or more eligible member roles, and append-only revisions. More than
one active template may target the same role; an Envelope request pins the
chosen template ID and revision.

Provisioned Envelope requests may include current-period spend usage. Usage is
the sum of the latest observation for each runtime bound to that Envelope
instance, plus active instance-scoped top-up grants in the effective limit. An
`available`, `partial`, or `unavailable` status is authoritative; presentation
must never guess a missing value. Request detail includes append-only status
history.

Capability metadata supplies tool access class and provider catalog
availability. The browser must not infer either from display text. Unsupported
models are omitted rather than rendered as guessed disabled options. Admission
remains the authority for exact deltas and Envelope revisions.

### Connections and onboarding

`GET /app/api/v1/connections` returns connected providers and the available
provider catalog. GitHub is enabled; unavailable providers remain explicit and
disabled. Provider `start` and `disconnect` mutations are browser-session and
CSRF scoped. The onboarding aggregate composes connection, Envelope, workflow,
and run evidence; dismissal is a server-side preference. When browser surfaces
are enabled and at least one execution binding is advertised, Steward seeds the
reserved immutable `repo-summary@1` read-only sample against a deployment-owned
agent. The renderer returns a deterministic suggested path. The workflow step
is complete when a run's caller-workflow provenance matches that stored path or
when the user explicitly acknowledges that the rendered file was added.

### Fleet and runs

PostgreSQL Task state is the browser run source of truth. User routes are
exact-owner scoped; administrator `all-runs` routes use browser administrator
authority and may expose the owner's display email. List responses include
phase facets computed from the current filter with the phase predicate removed.

Run detail exposes validated GitHub source provenance when it was captured at
submission, four bounded stages (admission, runtime provisioning, agent
execution, and finalization), and one execution step with stdout/stderr streams.
The separate log endpoint supports bounded byte-offset reads while a Task is
running and marks whether the stream is complete. Cancel is owner scoped.
Re-run creates a fresh Task only for a versioned Steward workflow; a
GitHub-triggered run fails closed until a governed provider dispatch can create
a new upstream run without inventing provenance.

### Federated Task subjects

The browser administrator API exposes read-only list/detail/audit operations at
`/admin/api/v1/federated-subjects` and revision-checked `associate`, `replace`,
and `disable` mutations below each subject ID. These APIs manage the exact
verified issuer/subject association only. They do not create a canonical user,
approve a User Envelope, or authorize a Task. Mutation actor identity always
comes from `BrowserAdminAuthority`; request bodies cannot name the actor.

An administrator should first inspect the observation and intended canonical
user, then submit `expectedRevision` and `canonicalUserId`. A `409` requires a
fresh read and review, not an automatic retry. Disabling may retain the former
canonical-user reference for audit while immediately preventing resolution.

## Deployment boundary

The chart routes browser API and authentication prefixes to the apiserver and
all presentation routes to the Next.js web service. The apiserver and web
service remain separately deployable, but there is one browser presentation
implementation.
