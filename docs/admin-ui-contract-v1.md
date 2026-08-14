# Steward administrator browser contract v1

Status: foundation contract. The Approvals, Envelope, and Fleet data APIs are
separate delivery slices. This document records the source-backed boundary they
must extend; it does not claim that their missing operations exist.

## Serving and authentication

The administrator dashboard is part of `steward-apiserver`, not a separately
deployed service. Its HTML, CSS, and JavaScript are compiled into the binary and
served below `/admin`. The existing chart exposes only the internal apiserver
`ClusterIP` service and defines no public Ingress, so adding these routes does
not create a public endpoint.

Every dashboard document, asset, and API request uses the existing
`RequestAuthenticator` and Kubernetes `TokenReview` path. A caller must have the
configured exact administrator group (the chart default is
`agents.apelogic.ai/admin`). A member-role, Task, runtime, provider, or the
route-scoped steward-run bootstrap identity is not administrator authority.

Steward does not accept a provider token in a URL, HTML document, Web Storage,
cookie, or JavaScript configuration. The local browser-session slice now defines
a direct Google OIDC callback and an opaque server-managed cookie boundary; see
`browser-session-contract-v1.md`. The production verifier is implemented, while
runtime activation remains fail-closed until the reviewed Google client metadata and secret are
injected. The existing TokenReview administrator shell and bootstrap in this document are
unchanged.

### Localhost human-acceptance harness

The repository includes an opt-in example for human review of this exact
browser contract. It is not an authentication mechanism or a deployment
surface. The example composes the same embedded assets, administrator
authentication middleware, and browser security headers as the production
router, but its authenticated mode injects one deterministic RFC 2606 test
identity inside the separately compiled example process. It does not construct
the Kubernetes, PostgreSQL, Jira, TLS, or Task collaborators, makes no external
request, and persists nothing.

The example is excluded from the default feature set and has
`required-features = ["admin-demo"]`. Production packaging builds only the
named `steward-apiserver-bin`, controller, and mint binaries; the Helm chart has
no invocation or configuration for this example. The process rejects every
non-loopback bind even when one is supplied explicitly.

Start authenticated and unauthenticated modes in separate terminals so the
human reviewer can inspect both real entry behaviors:

```bash
cargo run -p steward-apiserver --locked \
  --features admin-demo --example admin-dashboard-demo -- \
  --mode authenticated --bind 127.0.0.1:0

cargo run -p steward-apiserver --locked \
  --features admin-demo --example admin-dashboard-demo -- \
  --mode unauthenticated --bind 127.0.0.1:0
```

Each process prints its assigned loopback URL. `Ctrl-C` performs graceful
shutdown and releases the listener. A safe handoff for a human acceptance test
must record the exact PR head and both printed URLs, keep both processes alive
until the decision is recorded, and then stop them.

At desktop and narrow viewport, verify:

- authenticated shell, bootstrap identity, assets, and Approvals / Envelope /
  Fleet navigation;
- unauthenticated entry returns the real `401` bearer challenge rather than a
  fabricated dashboard state;
- keyboard tab selection and focus, refresh, and browser back/forward;
- loading, empty, fatal, and unauthorized presentation that the foundation
  currently implements;
- readable layout without clipping or unexpected overflow;
- no console errors in the authenticated flow; in the unauthenticated flow,
  the expected `401` resource failure and a browser fallback `/favicon.ico`
  probe may appear, but neither may trigger a non-loopback request;
- no unexpected non-loopback requests, cookies, `localStorage`, or
  `sessionStorage` entries.

Automated browser checks and the deterministic test identity do not replace the
human acceptance decision.

## Versioned browser API

The browser API prefix is `/admin/api/v1`. Its media type is JSON unless noted.
The Rust response types are included in Steward's OpenAPI document, which is the
typed source of truth.

### `GET /admin/api/v1/bootstrap`

Returns only the authenticated operator identity and the UI surfaces available
to this contract:

```json
{
  "apiVersion": "steward.admin/v1",
  "actor": "admin@example.com",
  "surfaces": ["approvals", "envelope", "fleet"]
}
```

The example uses an RFC 2606 identity and is not production data. An absent or
invalid bearer token returns `401`; a valid non-administrator returns `403`.

All `/admin` responses set `Cache-Control: no-store`, a self-only Content
Security Policy, `Referrer-Policy: no-referrer`, clickjacking protection,
content sniffing protection, and a restrictive Permissions Policy. The shell
contains no inline script or style and loads no third-party asset.

Future browser mutation routes under `/admin/api/v1` must remain inside the
shared mutation boundary. It requires all of the following before a handler can
run:

- `Origin` exactly equals `https://` plus the HTTP `Host` header;
- `Sec-Fetch-Site: same-origin`;
- `X-Steward-CSRF: 1`;
- `Content-Type: application/json`.

Bearer authentication still runs independently. These checks are additive and
must not be used as identity. Steward emits no CORS opt-in.

## Current source inventory and screen gaps

### Approvals

Existing source:

- `GET /admin/approvals` authenticates an administrator, calls
  `AdmissionLedger::pending_approvals`, and returns a minimal server-rendered
  HTML table.
- `POST /admin/approvals/{approval_id}/file` files or recovers the external
  decision reference.
- `POST /admin/approvals/{approval_id}/approve` validates evidence, applies an
  instance-bound grant, and records the authenticated actor.
- `PendingApproval` is backed by `approvals` joined to append-only
  `admission_decisions`. It contains the proposed absolute manifest, exact
  deltas, envelope revision, actor, member role, and decision evidence.

Missing for the dashboard screen:

- versioned JSON queue and detail responses;
- a rejection operation and its audited state transition;
- explicit concurrency material for a stale browser decision;
- presentation-safe decision status and recoverable error codes.

The Approvals screen must not parse the legacy HTML route or infer a rejection
write from the database schema.

### Envelope

Existing source:

- `POST /admin/envelopes/{member_role}` validates an `Envelope` and appends an
  immutable, strictly increasing revision authored by the authenticated actor.
- `AdmissionLedger::latest_envelope` and `PgStore::latest_envelope` can read the
  current revision, but no administrator GET route exposes it.
- Admission already returns exact `AdmissionDelta` counterexamples for a
  concrete runtime request.

Missing for the dashboard screen:

- role discovery and versioned current-envelope reads;
- a stage/prove operation for an uncommitted candidate;
- blast-radius/runtime impact data and an explicit narrowing disposition;
- optimistic concurrency binding to the current envelope revision;
- an apply response describing durable projection/reconciliation state.

The UI must not claim that OpenShell, MCP-GW, or LiteLLM updates are “hot” until
their observed reconciliation state is represented by a real API.

### Fleet

Existing source:

- `RuntimeRepository` supports point lookup and desired-state writes used by
  admission and approval. It has no list contract.
- Kubernetes `AgentRuntime.status` is the current lifecycle source of truth.
- PostgreSQL stores append-only runtime events and spend observations, not the
  current phase.

Missing for the dashboard screen:

- paginated runtime listing joined by `runtime_uid`;
- a typed summary for phase, owner, role/service scope, budget/spend, TTL,
  artifact revision, and reconciler findings;
- freshness/provenance for every derived field;
- stable filters and attention categories.

The Fleet screen must not manufacture totals, spend, expiry, drift, or upgrade
state from unavailable data. Current phase must continue to come from the CRD,
never from PostgreSQL history.

## Extension rules

- Add each data or mutation route to `/admin/api/v1`; do not change the meaning
  of the legacy routes in place.
- Define Rust request/response types with `deny_unknown_fields` where inputs are
  accepted and publish them in OpenAPI.
- Write the negative authorization, origin, CSRF, stale-revision, and
  secret-leak tests before the handler.
- Render untrusted values with text nodes, not HTML interpolation.
- Keep loading, empty, unavailable, and conflict states distinct.
- Preserve the checked-in mockup's information hierarchy, not its fictional
  people, agents, totals, or product guarantees.
