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

## HOP-1 claim contract v2

The protected header is `alg=EdDSA`, `typ=JWT`, and a public-key `kid`.

The payload contains the standard `iss`, `sub`, `aud`, `iat`, `exp`, and `jti`
claims. `azp` is always the validated workload SPIFFE ID. For a user principal,
`sub` is the opaque Steward canonical user ID and `email` is secondary verified display metadata.
A delegated service uses the same canonical-user subject while retaining its service name below.
A pure service keeps `service:<name>` for both `sub` and `email`; this is a service identifier for
gateway compatibility, not a human acting user. Mint must never use email as the person-bound
subject or provider-grant key.

Steward-specific claims live under `steward`:

- `version`: `2`
- `acting_as`: `user`, `service_for_user`, or `service`
- `service`: the service name for either service mode; absent for users
- `runtime_uid`: the immutable Kubernetes runtime UID
- `tools`: the exact `provider`, `resource`, and `action` attenuation

Changing or adding a claim changes this wire contract and requires coordinated
consumer updates under `crates/steward-mint/AGENTS.md`.
