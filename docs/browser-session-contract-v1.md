# Steward browser session contract v1

Status: Google code exchange and signed ID-token verification were released in Steward v0.3.20.
Configured-issuer headless JWT access-token acceptance on the application API is Unreleased. Both
paths are disabled unless their reviewed configuration and secret inputs are present.

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

The headless path accepts only a signed RS256 or ES256 JWT access token from the configured issuer.
An operator may require an exact protected JOSE `typ`, such as `at+jwt`; otherwise `typ` is
optional. A browser ID token is not interchangeable with an API access token because its audience
is a frontend client rather than the configured API audience. Provider `groups` claims are ignored;
the canonical user's existing local RBAC assignments remain the only role source.

## First local RBAC grant

There is no first-login administrator shortcut. An organization user signs in once, opens
`/settings`, and supplies the displayed opaque canonical user ID to an authorized Steward
operator. In the same protected runtime where `STEWARD_DATABASE_URL` is already projected, that
operator records the initial local grant with:

```sh
kubectl -n <namespace> exec deploy/steward-apiserver -- \
  /usr/local/bin/steward bootstrap-rbac \
  --user-id usr_<opaque-id> \
  --grant administrator \
  --actor <audited-operator>
```

The command requires all three values, validates the opaque user-ID shape, and appends a grant
event. It never accepts an email, Google subject, token, or unreviewed provider claim as authority;
revocation likewise appends a distinct local RBAC event.

The existing Kubernetes TokenReview administrator routes remain unchanged. Browser sessions are a
parallel, route-specific frontend boundary and cannot inject a bearer assertion into that operator
path.

The optional browser Connections path keeps GitHub mutations in governed,
short-lived `steward-connections` runtimes. Provider status calls and
non-GitHub starts such as Jira use a short-lived Mint token limited to one exact
provider action and call that provider's gateway route directly from the
apiserver. Identity authenticates the browser but does not issue authority to
browser JavaScript. Steward never places a bearer, provider credential, browser
session value, or role claim in browser JavaScript.

Production browser configuration parses and canonicalizes one real HTTPS origin. Userinfo,
non-root paths, query strings, fragments, malformed ports, whitespace, scheme-like strings, and
insecure schemes fail closed. The configured callback must then equal the canonical origin plus
exactly `/admin/auth/callback`.

## Routes

- Next.js owns `GET /admin/sign-in` and all other browser presentation routes.
- `GET /admin/auth/login` starts a five-minute one-time authorization flow. `returnTo` accepts
  only exact current Next.js destinations.
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

## Optional headless user access

When `browserAuth.userTokens.issuer` and `hyperShellAudience` are nonempty, a native or third-party
frontend may call only `/app/api/v1/*` with `Authorization: Bearer <access-token>`. Steward fetches
bounded discovery and JWKS documents from that exact HTTPS issuer, follows no redirects, and
accepts only RS256 or ES256 signatures. It requires the configured HyperShell audience, one
allowlisted `azp` or `client_id`, an email in the browser workspace domain, a valid key ID, current
`exp` and `nbf` with bounded clock skew, and an `iat` within `maximumTokenAgeSeconds`.
`userIdClaim` selects the stable person identifier and defaults to `sub`; `emailClaim` selects the
email claim. `email_verified` is reserved and cannot be selected for either identity claim. A token
missing either configured claim is refused. By default `email_verified` must be true;
`trustEmail=true` is an explicit operator decision to trust the configured issuer's email. An
explicit `email_verified=false` is always refused, including for a trusted issuer.
If `requiredTyp` is nonempty, the JOSE `typ` must exactly match it. Opaque tokens fail closed.

The optional portal audience is accepted only when `acceptPortalAudience=true`, a nonempty
`portalAudience` is configured, and the frontend client ID is allowlisted. It does not replace the
HyperShell audience or relax the client and age checks. Empty user-token values retain the existing
cookie-only behavior.

Google user access tokens are opaque, so Google sign-in remains browser-session only. Google ID
tokens are not API bearer credentials: their client-ID audience does not satisfy the configured
HyperShell audience. Opaque-token introspection or exchange is outside this contract.

The verified subject is resolved or registered through the canonical identity resolver. Google
browser sign-in never attaches a new subject to an active user's email. A trusted-email
access-token issuer may attach its exact subject to an existing canonical user only when the
organization and workspace email domain match and that user has no other subject from the same
issuer. The attachment records actor `user-token:<issuer>` and does not create a second user. An
access-token issuer that still requires `email_verified` cannot use email linking. Access still
comes only from Steward's local RBAC assignments; token claims do not grant administrator or
member roles. Every accepted bearer identity writes one request audit record with the canonical
user, verified issuer, allowlisted frontend client ID, HTTP method, and path, including requests
later refused for missing browser CSRF proof. Browser Tasks persist the same issuer and frontend
client ID through their existing caller reference. The raw token is never logged or stored. The
request binding is a SHA-256-derived opaque value and never exposes the token.

`browserAuth.autoCreateUsers` defaults to `true`, preserving existing first-sign-in registration.
When disabled, neither authentication path creates a new canonical user. Both paths may activate
an exact pending invitation. Browser sign-in otherwise requires an already registered exact
issuer/subject and never email-links an active user. A bearer from an issuer configured with
`trustEmail=true` may still attach to an existing active user under the constraints above; all
other previously unseen bearer identities are refused.

A bearer-only mutation does not need browser CSRF proof. If a request also contains a browser
cookie, the cookie remains subject to the existing origin, fetch-site, content-type, and CSRF
checks. If cookie and bearer resolve to different people, Steward returns 401. User bearer tokens
never authenticate `/admin/*`, operator, hook, or direct Task-token routes.
Authorization headers outside `/app/api/v1/*`, and non-Bearer schemes, do not change cookie-session
behavior.

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

The production activation consumes reviewed non-secret Google metadata, including the exact
client ID, HTTPS browser origin and callback, hosted domain, and organization identifier. The
Google client credential is projected from an operator-selected secret reference, for example
`/operator/steward-google-oidc`, under exactly the
Kubernetes key `client-secret`. Its value is one provider-issued raw plaintext scalar: no JSON
wrapper, quoting, Base64 transform, surrounding whitespace, or trailing newline. The raw scalar is
passed only to `GoogleOidcProvider::new` and is never included in errors or logs.

No `session-key` Kubernetes key, session-key environment variable, or
`/operator/steward-session-key` reference is part of the product contract. Its absence must not
block initial activation.

The apiserver activates the Google browser surface only when
`STEWARD_GOOGLE_OIDC_CLIENT_ID` is present. It then requires
`STEWARD_BROWSER_ORIGIN`, `STEWARD_GOOGLE_WORKSPACE_DOMAIN`,
`STEWARD_ORGANIZATION_ID`, and `STEWARD_GOOGLE_OIDC_CLIENT_SECRET`; incomplete configuration
stops rather than exposing an unverifiable sign-in route. Illustrative values are origin
`https://steward.example.test`, callback
`https://steward.example.test/admin/auth/callback`, Workspace domain `example.test`, and opaque
organization ID `org_example`. This opaque ID is chosen and kept stable by the
Steward operator; it is not a Google, cloud, or identity-provider organization
identifier. It starts with `org_` and uses only lowercase ASCII letters,
digits, `_`, or `-`, with a maximum length of 64 characters. Client ID and the
origin/domain/organization values are non-secret runtime configuration; the
client secret remains secret-projected only.
