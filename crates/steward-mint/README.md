# Steward HOP-1 mint

This crate is the signing-key boundary for Steward's short-lived HOP-1 tokens.
The token endpoint accepts OpenShell's OAuth client-assertion request, validates
the JWT-SVID through SPIRE's Workload API, resolves current Steward authority,
and only then signs a token.

## HOP-1 claim contract v1

The protected header is `alg=EdDSA`, `typ=JWT`, and a public-key `kid`.

The payload contains the standard `iss`, `sub`, `aud`, `iat`, `exp`, and `jti`
claims. For a user principal, `sub` and `email` are the acting user's email and
`azp` is the validated workload SPIFFE ID.

Steward-specific claims live under `steward`:

- `version`: `1`
- `acting_as`: `user`
- `runtime_uid`: the immutable Kubernetes runtime UID
- `tools`: the exact `provider`, `resource`, and `action` attenuation

Changing or adding a claim changes this wire contract and requires coordinated
consumer updates under `crates/steward-mint/AGENTS.md`.
