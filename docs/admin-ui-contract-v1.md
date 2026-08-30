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

The Next.js `/admin/approvals` page consumes
`GET /admin/api/v1/approvals` and the versioned decision APIs. Approval
evidence remains bound to the authenticated actor and exact runtime UID. The
browser must not infer state transitions from database shape.

### Envelopes

The Next.js envelope administration pages consume versioned JSON template and
request APIs. Admission remains the authority for exact counterexamples and
envelope revisions. Presentation must not claim that OpenShell, MCP-GW, or
LiteLLM updates are observed until the API represents their reconciliation
state.

### Fleet and runs

Kubernetes `AgentRuntime.status` remains the current lifecycle source of
truth. PostgreSQL holds append-only history and observations, not current
phase. Browser run views consume the versioned run and timeline APIs and must
preserve provenance and data-availability distinctions.

## Deployment boundary

The chart routes browser API and authentication prefixes to the apiserver and
all presentation routes to the Next.js web service. The apiserver and web
service remain separately deployable, but there is one browser presentation
implementation.
