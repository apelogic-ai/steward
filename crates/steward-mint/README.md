# Steward HOP-1 mint

This crate is the signing-key boundary for Steward's short-lived HOP-1 tokens.
The token endpoint accepts OpenShell's OAuth client-assertion request, validates
the JWT-SVID through SPIRE's Workload API, resolves current Steward authority,
and only then signs a token.

The same endpoint also dispatches the exact `inference` scope to a
runtime-credential resolver after those identity and live-authority checks.
That path returns the opaque bearer credential stored in a Kubernetes Secret
named by the immutable runtime UID and colocated in the runtime's namespace.
The Secret must carry the matching `agents.apelogic.ai/runtime-uid` label, be
controller-owned by the `AgentRuntime` with that UID in the same namespace, and
contain an `access-token` data entry. The credential is never placed in HOP-1
claims, Postgres, or AgentRuntime status, and the mint's token wrapper
implements neither `Debug` nor `Display`.

Any request containing the `inference` scope fails closed unless a resolver
returns a bound credential. It never falls back to HOP-1. The response
`expires_in` remains the authority TTL so OpenShell's token-grant cache
re-verifies authority on the same clock even though key deletion is the
immediate revocation mechanism.

Consumers validate the signature and claims, then call `POST /introspect` for
every request. Introspection re-resolves the workload's current authority and
returns only `{"active":true|false}`. Revocation, suspension, termination, or a
binding change therefore invalidates an already-issued token immediately; its
short TTL is a backstop, not the revocation boundary. Introspection failures
must fail closed. The route requires the configured gateway credential as an
`Authorization: Bearer` value before it parses the presented token or resolves
authority. The mint retains only the credential's SHA-256 digest.

The separate `POST /control-plane/token` route supports one runtime-free,
metadata-only operation: Steward API reading the authenticated browser user's
GitHub connection status. The caller presents a projected Kubernetes service
account token. The mint verifies it with Kubernetes TokenReview against the
configured audience and exact Steward API service-account identity before
accepting the typed browser `Principal` and canonical authority. Callers cannot
select a service, scope, tool, audience, or TTL. The resulting HOP-1 token is
limited to `github/provider-control/status`, expires in at most 15 seconds, and
is never accepted for provider mutation or general tool use.

Unlike runtime HOP-1, this control-plane token has no `AgentRuntime` whose live
authority can be re-resolved. Introspection therefore accepts only its signed,
unexpired, exact fixed-purpose claim shape. Its 15-second lifetime is the
revocation bound; it is intended for one status request and must not be cached
as provider authority.

## HOP-1 claim contract v3

The protected header is `alg=EdDSA`, `typ=JWT`, and a public-key `kid`.

The payload contains the standard `iss`, `sub`, `aud`, `iat`, `exp`, and `jti`
claims. `azp` is always the validated workload SPIFFE ID. For a user principal,
`sub` is the immutable `CanonicalUserId` from the live runtime's typed
`canonicalAuthority`, while `email` is verified display metadata and never an
authorization key. A delegated service uses the same canonical user subject while
retaining its service name below. A pure service uses
`service:<name>` for both `sub` and `email`; this is a service identifier for
gateway compatibility, not a human acting user.

No organization, identity-provider subject, or provider name is serialized into
HOP-1. Consumers isolate user grants by the exact `(iss, sub, provider)` tuple and
must not infer or adopt identity from `email`. A person-bound runtime without a
valid canonical authority is a legacy runtime that must reconnect; it cannot mint.
Introspection revalidates the stable canonical subject but deliberately does not
revoke a token merely because the user's verified display email changed.

Steward-specific claims live under `steward`:

- `version`: `3`
- `acting_as`: `user`, `service_for_user`, or `service`
- `service`: the service name for either service mode; absent for users
- `runtime_uid`: the immutable Kubernetes runtime UID for runtime tokens;
  omitted only for the fixed-purpose control-plane status token
- `purpose`: omitted for runtime tokens; exactly
  `provider_connection_status` for the control-plane status token
- `tools`: the exact `provider`, `resource`, and `action` attenuation

The control-plane status token uses `acting_as=service_for_user`,
`service=steward-apiserver`, the browser user's canonical ID in `sub`, and only
the `github/provider-control/status` tool grant. Ordinary runtime-token claims
and their online authority revalidation are unchanged.

Changing or adding a claim changes this wire contract and requires coordinated
consumer updates under `crates/steward-mint/AGENTS.md`.
