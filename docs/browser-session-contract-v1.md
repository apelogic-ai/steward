# Steward browser session contract v1

Status: local callback/session slice plus production Google code exchange and signed ID-token
verification. Runtime activation remains fail-closed until LBE-235 supplies the Google web-client
metadata and client secret through the reviewed configuration and secret paths.

## Identity boundary

The browser boundary is direct Google organization OIDC. Production configuration accepts only:

- issuer `https://accounts.google.com`;
- authorization-code flow with state, nonce, and PKCE S256;
- one exact HTTPS browser origin and its exact `/admin/auth/callback` URI;
- `openid email profile` scopes and one exact Google hosted-domain (`hd`) value;
- a signed ID token whose immutable Google `sub`, verified email, and hosted domain pass
  `OrganizationIdentityPolicy`.

The resolver maps that validated external subject to Steward's opaque `CanonicalUserId`. Email is
display metadata and never a session, ownership, or provider-grant key. Steward then reads its
append-only local RBAC assignment ledger by that exact opaque ID. An unassigned person receives
the ordinary user role with no member-role access; administrator and member-role grants require
explicit local grant events, and revocation appends a new event. Google email or hosted-domain
membership does not imply any Steward authority. This boundary has no AWS or Kubernetes identity
dependency.

The existing Kubernetes TokenReview administrator routes remain unchanged. Browser sessions are a
parallel, route-specific frontend boundary and cannot inject a bearer assertion into that operator
path.

Production browser configuration parses and canonicalizes one real HTTPS origin. Userinfo,
non-root paths, query strings, fragments, malformed ports, whitespace, scheme-like strings, and
insecure schemes fail closed. The configured callback must then equal the canonical origin plus
exactly `/admin/auth/callback`.

## Routes

- `GET /admin/sign-in` renders a self-hosted sign-in shell.
- `GET /admin/auth/login` starts a five-minute one-time authorization flow. `returnTo` accepts only
  `/admin/connections` or the local hand-test completion page.
- `GET /admin/auth/callback` consumes the flow cookie and state before code exchange, verifies the
  returned nonce and organization identity, resolves a canonical principal, rotates any supplied
  session cookie, and redirects to the allowlisted path.
- `GET /admin/api/v1/session` returns `steward.browser-session/v1`: bounded canonical principal,
  `user` or `admin` role, the caller's locally assigned member roles, allowed surfaces, and the
  current per-session CSRF proof.
- `POST /admin/auth/logout` revokes the server-side session and expires its cookie.

The cookie contains only an unguessable opaque server-side lookup value. Identity, role, provider
claims, CSRF proof, OAuth codes, and tokens are not stored in the cookie, browser storage, or URL.
The deployed session cookie uses the `__Host-` prefix, `Secure`, `HttpOnly`, `Path=/`,
`SameSite=Lax`, a one-hour TTL, and server-side revocation. The OAuth flow cookie is separate, uses
the `__Secure-` prefix so it can remain path-limited to `/admin/auth`, and expires after five
minutes. Local HTTP uses clearly local non-`Secure` cookie names because a browser will not return a
`Secure` cookie over loopback HTTP.

For the initial DEV/E2E activation, the registry is process-local and the Steward apiserver runs
exactly one replica. A process restart or rollout deliberately drops every session and pending
authorization flow, so the browser must sign in again. A fresh process cannot accept a cookie or
flow handle minted by the prior process. There is no browser-session signing or encryption key:
the cookie is already an opaque lookup handle, and all authority remains in server-side state.

Multi-replica continuity is post-E2E work tracked by LBE-248. It requires a shared server-side
session store with atomic one-time flow consumption, TTL, revocation, and fixation rotation. A
signing key or encrypted client-side session would not provide those semantics and is not an
acceptable substitute.

The process-local registry admits at most 256 pending authorization flows and 4,096 browser
sessions. Before each insertion it prunes entries whose TTL expires at or before the current time
while holding the same lock used for the capacity check and insertion. A full registry rejects the
new flow or session with a generic service-unavailable response; it never evicts a live entry or
grows beyond the cap.

Every authenticated browser mutation requires all of:

- exact configured `Origin`;
- `Sec-Fetch-Site: same-origin`;
- JSON content type;
- `X-Steward-CSRF` equal to the current server-side session value.

Protected application handlers receive `BrowserSessionContext`. It contains a canonical principal
and an opaque `BrowserSessionBinding` that is cloneable, comparable, and hashable but is not
debuggable, displayable, serializable, or exposed as a string. Connections can key one-time provider
continuations by that binding without putting a session identifier into JSON.

Ordinary user routes use `protect_browser_routes`. Browser administrator routes must instead use
`protect_browser_admin_routes` and extract the unforgeable `BrowserAdminAuthority` extension.
Missing browser authentication returns 401, an authenticated ordinary user returns 403, and only a
session resolved with the administrator role receives the typed authority. A Kubernetes
TokenReview bearer cannot satisfy this browser guard, and a browser cookie cannot satisfy the
separate operator TokenReview boundary.

## Local fake OIDC hand test

The fake provider is compiled only with the existing `admin-demo` feature, accepts only an explicit
loopback bind, uses neutral deterministic identities, and drives the same login, callback, canonical
claim policy, session, CSRF, logout, and security-header code as the production router. It performs
no external request and holds no secret.

Run the user and administrator fixtures separately:

```bash
cargo run -p steward-apiserver --locked \
  --features admin-demo --example admin-dashboard-demo -- \
  --mode oidc-user --bind 127.0.0.1:0

cargo run -p steward-apiserver --locked \
  --features admin-demo --example admin-dashboard-demo -- \
  --mode oidc-admin --bind 127.0.0.1:0
```

Each process prints `/admin/sign-in`. Complete the redirect, inspect the bounded session response,
then check that cookies are HttpOnly, browser storage is empty, requests stay on loopback, refresh
preserves the session, and a cross-origin or missing-CSRF logout is rejected. Stop both processes
after the hand-test decision.

## Production verifier and activation boundary

`GoogleOidcProvider` is the production provider selected by LBE-239. It uses the existing bounded
rustls `reqwest` client for discovery, authorization-code exchange, and JWKS retrieval, and the
workspace-pinned `jsonwebtoken 11.0.0` `aws_lc_rs` backend for RS256 verification. Discovery and
all advertised endpoints are pinned to Google's exact HTTPS issuer/host/path contract. Redirects
are disabled; connect, total request, and response-body sizes are bounded.

The verifier requires one bounded key ID, rejects embedded/remote key headers and algorithm
confusion, accepts exactly one compatible RSA verification key, and validates the exact issuer,
singleton client audience, optional exact authorized party, consumed nonce, expiry and issue time,
bounded subject, verified email, and exact hosted domain. JWKS uses bounded HTTP caching, performs
one synchronized refresh on an unknown key, preserves a fresh last-good set across malformed
rotation responses, and fails closed after expiry if refresh is unavailable.

Unknown-key JWKS refresh, including any prerequisite discovery refresh, is serialized. A failed
refresh is remembered for its observed JWKS cache generation and suppresses duplicate waiter
retries for five seconds; a later generation is not suppressed. Cache time is recomputed after
waiting for the refresh gate and again when storing a response, so a key set that expires while
queued cannot be returned as fresh.

`GoogleOidcProvider::new` requires the client secret as a non-debuggable runtime value. The
non-secret client ID is also runtime configuration; neither value has a source default. No
authorization code, PKCE verifier, client secret, access/refresh/ID token, cookie, raw claim, or
provider body is included in errors or logs. Access and refresh tokens returned by Google are
ignored and not retained. Tokeninfo, UserInfo, unsigned parsing, and offline access are not used.

The earlier `GoogleAuthorizationOnlyProvider` remains intentionally fail-closed for callers that
have not supplied the production verifier configuration. Choosing it cannot silently enable a
partially verified login.

## DEV runtime and secret projection handoff

The production activation consumes the reviewed non-secret Google metadata, including the exact
client ID, HTTPS browser origin and callback, hosted domain, and organization identifier. The
Google client credential is projected from `/apelogic/dev/steward-google-oidc` under exactly the
Kubernetes key `client-secret`. Its value is one provider-issued raw plaintext scalar: no JSON
wrapper, quoting, Base64 transform, surrounding whitespace, or trailing newline. The raw scalar is
passed only to `GoogleOidcProvider::new` and is never included in errors or logs.

No `session-key` Kubernetes key, session-key environment variable, or
`/apelogic/dev/steward-session-key` reference is part of the product contract. The already-created
DEV container remains empty and unprojected until the separately reviewed Infra cleanup in
LBE-247. Its absence must not block initial activation.
