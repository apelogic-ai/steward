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

## Provider-token grant diagnostic contract

The `/token` route preserves its existing OAuth status and `error` member. For
a terminal token-grant rejection after Mint has accepted the request, it adds
exactly one optional JSON member:

```json
{"error":"invalid_client","steward_token_grant_outcome":"mint_invalid_client"}
```

`steward_token_grant_outcome` is a finite, request-independent string. It is
the only Mint diagnostic surface for this exchange. It never contains a token,
JWT-SVID, SPIFFE ID, runtime UID, header, response body, credential, or
request-derived text.

| Outcome | Emitting stage | Meaning |
| --- | --- | --- |
| `mint_unreachable` | OpenShell caller | No request reached Mint (transport/DNS/TLS). Mint cannot emit this outcome. |
| `mint_invalid_client` | Mint | JWT-SVID or client assertion was rejected. |
| `mint_authority_rejected` | Mint | The validated workload cannot resolve to active Steward authority. |
| `mint_audience_rejected` | Mint | The token audience was rejected. |
| `mint_scope_rejected` | Mint | The requested scope was rejected. |
| `mint_unavailable` | Mint | SPIRE or authority lookup was temporarily unavailable. |
| `token_handoff_failed` | Mint or OpenShell caller | A required short-lived credential could not be returned or installed. |
| `unexpected` | Mint or OpenShell caller | A non-classified failure; callers must fail closed. |

The governed GitOps E2E consumes only this finite field when OpenShell
propagates its token endpoint response into its structured task failure. It
must not query Mint directly, inspect pod logs, or capture headers/bodies. If
the field is absent or outside this table, the E2E records only `unexpected`
and fails closed. An OpenShell transport or post-response injection failure
uses the same enum at its own stage; Mint must not fabricate either outcome.

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
`sub` and `email` are the acting user's email. A delegated service uses the same
user subject while retaining its service name below. A pure service uses
`service:<name>` for both `sub` and `email`; this is a service identifier for
gateway compatibility, not a human acting user.

Steward-specific claims live under `steward`:

- `version`: `2`
- `acting_as`: `user`, `service_for_user`, or `service`
- `service`: the service name for either service mode; absent for users
- `runtime_uid`: the immutable Kubernetes runtime UID
- `tools`: the exact `provider`, `resource`, and `action` attenuation

Changing or adding a claim changes this wire contract and requires coordinated
consumer updates under `crates/steward-mint/AGENTS.md`.
