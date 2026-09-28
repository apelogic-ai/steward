# PC-S01: Runtime-free provider connection status

Priority: P0 operator UX and reliability fix

Status: ready for architecture approval and implementation

## Goal

Make the Steward Connections page read the authenticated user's provider connection
metadata without creating a Task, `AgentRuntime`, OpenShell sandbox, or provider
probe. The page must render immediately, use MCP-GW's durable lifecycle metadata when
it is available quickly, and always offer an authorization or reauthorization action
when status is unknown or unhealthy.

This ticket does not change MCP-GW. MCP-GW 0.4.9 already owns the required normalized
connection lifecycle and metadata-only status endpoint.

## Current defect

Steward currently handles `GET /admin/api/v1/connections/github` through
`GovernedConnectionsBroker::status_operation`. A cache miss reserves an internal
connection operation and Task, waits for the controller to provision a one-shot
OpenShell runtime, runs `steward-connections-bridge github.status`, reads
`GET /connections/github/status`, and then tears the runtime down.

The five-second cache makes an ordinary page visit effectively cold whenever the
previous status read is more than five seconds old. The request may wait for the
40-second connection-operation deadline. A disconnect conflict path also forces an
uncached status operation. When the operation fails, the generic resource boundary
hides the provider card and its recovery action.

PR 97 added exact active and renewal credential expiry to the bridge and UI. It did
not change this execution path; its PR contract explicitly retained the five-second
cache and governed cold-start latency.

## Existing MCP-GW contract

MCP-GW 0.4.9 provides:

- `GET /connections/github/status` as one datastore read that neither decrypts
  credentials nor calls GitHub;
- `POST /connections/github/authorize` for initial authorization and reauthorization,
  including while already connected;
- `POST /connections/github/refresh`; and
- `POST /connections/github/disconnect`.

Every route requires the same trusted HOP-1 identity used by later MCP tool calls.
Connection custody is keyed by provider, HOP-1 issuer, and HOP-1 subject. A direct
status reader must therefore use the existing Steward Mint issuer and canonical user
subject; a browser cookie, Google ID token, new issuer, or email join would address a
different principal and is forbidden.

## Proposed implementation boundary

### Narrow control-plane HOP-1 exchange

Add a Steward Mint exchange intended only for the authenticated Steward control
plane. It must:

- authenticate the exact `steward-apiserver` workload identity;
- accept a typed `Principal` and canonical authority derived by the apiserver from the
  authenticated browser session, never a bare acting-user email;
- issue from the existing Steward Mint issuer with the canonical user ID as `sub`;
- bind the fixed Steward connections service identity and status-only internal
  authority;
- use a lifetime bounded to one immediate request and no longer than 15 seconds;
- remain eligible for MCP-GW's existing authenticated introspection; and
- reject every other workload, audience, service, action, subject shape, or delegated
  identity.

The apiserver must never receive the Mint signing key. The returned bearer is held in
memory only, is never persisted or logged, and is discarded after the single bounded
status request.

This is a change under `crates/steward-mint/` and therefore requires the repository's
explicit advance maintainer approval before implementation.

### Direct metadata reader

Add a provider-connection metadata port and implement the MCP-GW adapter with an exact
allowlist:

```text
GET <operator-configured MCP-GW origin>/connections/github/status
```

The adapter must not expose a generic HTTP request interface. It must reject redirects,
userinfo, query parameters, fragments, oversized bodies, unknown fields, invalid
phases, malformed timestamps, connected responses without a verified account, and
connected responses with missing scopes. It preserves only bounded lifecycle metadata
and never credential material.

The complete server-side read has a hard one-second deadline and no retry loop on the
browser request path. MCP-GW remains the source of truth; Steward does not query
GitHub, decrypt credentials, refresh credentials, or infer live provider health.

### Existing private-route trust boundary

MCP-GW authenticates the HOP-1 identity on its private connection routes, but it does
not interpret Steward's internal provider-control action grant to authorize each HTTP
method and path. Under the no-MCP-GW-change constraint, the trusted control-plane
boundary is therefore enforced by all of the following together:

- only the exact apiserver workload may obtain the short-lived control-plane bearer;
- the apiserver derives the principal only from its authenticated session;
- the adapter exposes only the fixed status `GET` and no generic request primitive;
- the bearer stays in process for at most one request and 15 seconds;
- network policy permits only the required private service traffic; and
- browser input cannot influence the destination, method, path, headers, or bearer.

The ticket must not claim that the bearer is cryptographically scoped to one MCP-GW
route. If that stronger property becomes required, MCP-GW must enforce route-specific
authority in a separately reviewed change. This limitation is acceptable only for the
trusted, private, short-lived status reader described here and does not authorize
moving mutations onto the same direct path.

### Immediate browser behavior

The Connections page renders the GitHub card before metadata resolves.

- Healthy connected metadata shows green and the reported active and renewal expiry.
- An approaching effective reauthorization deadline shows a warning and a prominent
  **Re-authorize GitHub** action.
- Expired, missing-scope, or `reauthorization_required` metadata shows an unhealthy
  state and a prominent **Re-authorize GitHub** action.
- Disconnected metadata shows **Authorize GitHub**.
- A timeout, transport failure, malformed response, or absence of prior metadata
  shows no account, scope, expiry, or health claim and keeps an
  **Authorize / re-authorize GitHub** action available.
- A healthy connection retains a secondary manual **Re-authorize GitHub** action so
  an operator can recover from a provider-side revocation not yet observed by
  lifecycle metadata.

Expiry timestamps drive the displayed countdown locally; credential expiry is not a
cache TTL. Revocation, missing scopes, and provider authentication failures can change
before an expiry deadline.

The Connections surface must not use the generic all-or-nothing resource boundary for
status availability. Authentication and authorization failures still fail closed;
metadata unavailability only changes what the card can claim.

## Scope

- add the narrowly scoped apiserver workload and Mint exchange contract;
- add a direct, bounded MCP-GW connection-status adapter;
- route browser status reads through that adapter rather than
  `GovernedConnectionsBroker::reserve`;
- remove status reads from active connection-operation planning and the new-runtime
  path;
- remove the five-second runtime-result cache from the browser status path;
- preserve current authenticated browser-session isolation;
- render the provider card and recovery action during loading and failure;
- incorporate the always-available manual reauthorization behavior tracked by PR 96;
- add Helm configuration and network policy for apiserver-to-Mint and
  apiserver-to-private-MCP-GW traffic; and
- retain immutable historical internal-authority documents and applied migrations
  unchanged.

## Explicit non-goals

- no MCP-GW source, schema, image, or release change;
- no GitHub live-health probe from the Connections page;
- no credential decryption, renewal, or provider API call during status;
- no browser-selected issuer, subject, endpoint, provider, action, or authority;
- no exposure of a HOP-1 bearer to browser JavaScript;
- no change to ordinary agent MCP authentication or provider-credential custody;
- no change to frozen `steward.m1/v1`; and
- no requirement in this ticket to remove OpenShell from explicit authorize,
  reauthorize, refresh, or disconnect mutations.

## Red-first negative proofs

Begin with tests proving the present escape paths:

1. An authenticated status request with no cached result currently reserves a
   connection operation and Task; the replacement test requires zero inserted Tasks,
   connection operations, and AgentRuntimes.
2. An MCP-GW hang currently holds the browser request on the governed deadline; the
   replacement must return or fall back within one second.
3. Metadata unavailability currently hides the recovery action; the replacement must
   render the GitHub card and authorization action immediately.
4. A caller-controlled subject, email, issuer, origin, route, or action must never
   affect the Mint exchange or outbound MCP-GW request.
5. Another canonical user must not read the first user's connection metadata.
6. A control-plane status bearer must be rejected after its short expiry and must not
   become an ordinary agent credential.
7. Credential-shaped or unknown MCP-GW fields must be rejected and must never reach
   API responses, logs, traces, or persisted records.

## Verification

- focused Mint exchange and introspection tests;
- apiserver broker tests proving status cannot call `reserve`;
- adapter contract tests against the exact MCP-GW 0.4.9 response shape;
- browser tests for loading, healthy, expiring, expired, disconnected,
  `reauthorization_required`, unavailable, and manual reauthorization states;
- real pinned MCP-GW 0.4.9 integration showing the same issuer and canonical subject
  resolve the existing connection record; and
- an ephemeral-cluster E2E that records the AgentRuntime and Task sets before and
  after repeated cold Connections-page loads and proves they are unchanged.

Run one heavy local integration lane at a time. The GitOps-owned local-main cluster is
not a development test target.

## Exit criteria

- opening or refreshing the Connections page never creates a Task, connection
  operation, `AgentRuntime`, OpenShell workspace, or sandbox;
- MCP-GW status is read directly and returns exact expiry metadata when available;
- the UI becomes usable immediately and never waits on the 40-second governed
  operation deadline;
- missing or unavailable metadata produces no invented status and leaves an
  authorization or reauthorization action available;
- no page load calls GitHub or attempts credential renewal;
- explicit connection mutations retain their existing behavior; and
- the full repository gate and the named real-stack E2E are green without warnings.

## Follow-up boundary

A separate decision may move explicit authorize, reauthorize, refresh, and disconnect
mutations off one-shot OpenShell runtimes. That work must preserve CSRF enforcement,
typed consent, action-specific authority, audit evidence, idempotency, and fail-closed
provider cleanup. It is not required to remove the status defect and must not delay
this P0 ticket.
