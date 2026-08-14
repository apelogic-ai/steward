# Steward browser session contract v1

Status: local callback/session slice. Production Google code exchange and signed ID-token
verification remain fail-closed until LBE-239 selects and explicitly approves a verifier and
LBE-235 supplies the Google web-client registration through the secret path.

## Identity boundary

The browser boundary is direct Google organization OIDC. Production configuration accepts only:

- issuer `https://accounts.google.com`;
- authorization-code flow with state, nonce, and PKCE S256;
- one exact HTTPS browser origin and its exact `/admin/auth/callback` URI;
- `openid email profile` scopes and one exact Google hosted-domain (`hd`) value;
- a signed ID token whose immutable Google `sub`, verified email, and hosted domain pass
  `OrganizationIdentityPolicy`.

The resolver maps that validated external subject to Steward's opaque `CanonicalUserId`. Email is
display metadata and never a session, ownership, or provider-grant key. Ordinary users receive the
user role by default. Administrator role assignment is an explicit allowlist of canonical user IDs;
Google email or hosted-domain membership does not imply administrator authority.

The existing Kubernetes TokenReview administrator routes remain unchanged. Browser sessions are a
parallel, route-specific frontend boundary and cannot inject a bearer assertion into that operator
path.

## Routes

- `GET /admin/sign-in` renders a self-hosted sign-in shell.
- `GET /admin/auth/login` starts a five-minute one-time authorization flow. `returnTo` accepts only
  `/admin/connections` or the local hand-test completion page.
- `GET /admin/auth/callback` consumes the flow cookie and state before code exchange, verifies the
  returned nonce and organization identity, resolves a canonical principal, rotates any supplied
  session cookie, and redirects to the allowlisted path.
- `GET /admin/api/v1/session` returns `steward.browser-session/v1`: bounded canonical principal,
  `user` or `admin` role, allowed surfaces, and the current per-session CSRF proof.
- `POST /admin/auth/logout` revokes the server-side session and expires its cookie.

The cookie contains only an unguessable opaque server-side lookup value. Identity, role, provider
claims, CSRF proof, OAuth codes, and tokens are not stored in the cookie, browser storage, or URL.
Deployed cookies use the `__Host-` prefix, `Secure`, `HttpOnly`, `Path=/`, `SameSite=Lax`, a one-hour
TTL, and server-side revocation. The OAuth flow cookie is separate, path-limited to `/admin/auth`,
and expires after five minutes. Local HTTP uses clearly local non-`Secure` cookie names because a
browser will not return a `Secure` cookie over loopback HTTP.

Every authenticated browser mutation requires all of:

- exact configured `Origin`;
- `Sec-Fetch-Site: same-origin`;
- JSON content type;
- `X-Steward-CSRF` equal to the current server-side session value.

Protected application handlers receive `BrowserSessionContext`. It contains a canonical principal
and an opaque `BrowserSessionBinding` that is cloneable, comparable, and hashable but is not
debuggable, displayable, serializable, or exposed as a string. Connections can key one-time provider
continuations by that binding without putting a session identifier into JSON.

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

## Deliberate production blocker

`GoogleAuthorizationOnlyProvider` constructs the exact Google authorization request but always
rejects code exchange. It must not be wired as a working production login. A production provider
must cryptographically verify Google's signed ID token, including signature/JWKS rotation,
issuer, audience/authorized-party, nonce, expiry/time, verified email, and hosted domain. `reqwest`
alone, UserInfo-only validation, Google's debugging `tokeninfo` endpoint, or parsing an unsigned JWT
does not meet this boundary. LBE-239 owns the explicit dependency/adapter decision.
