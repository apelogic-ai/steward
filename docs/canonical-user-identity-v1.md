# Canonical user identity v1

Steward uses `steward/canonical-principal/v1` as the stable person-identity contract. Its
authority key is an opaque `usr_<32 lowercase hex>` user ID allocated by Steward. Email,
identity-provider issuer, subject, and organization display values are not ownership keys.

## DEV organization trust boundary

The supported DEV browser identity provider is direct Google organization OIDC. The validating
browser/session boundary must require all of these before constructing an `OrganizationIdentity`:

- issuer exactly `https://accounts.google.com`;
- the configured immutable Google `sub` claim;
- `email_verified=true`;
- the Google hosted-domain (`hd`) claim exactly equal to the configured organization domain;
- the verified email domain equal to that same hosted domain.

`OrganizationIdentityPolicy` performs the exact issuer, hosted-domain, verification, and email
boundary checks. `OrganizationIdentity` is the already-validated, provider-neutral input to the
identity store. Raw ID tokens, authorization codes, cookies, and provider credentials are never
accepted by or returned from the store contract.

## Exact resolution and registration

`PgStore::resolve_canonical_identity` resolves only the complete reviewed tuple:

```text
(issuer, subject, organization_claim, organization_id) -> canonical user ID
```

The current verified email must also match both the canonical user record and subject mapping.
It detects drift but never discovers or adopts a person. A new subject with an existing email is
`CanonicalIdentityAmbiguousEmail`, not a match.

First registration allocates a random opaque user ID and records an audit event. An explicitly
reviewed issuer migration uses `attach_canonical_identity_subject(user_id, ...)`; it names the
existing opaque user ID and preserves it. An explicitly reviewed email rename uses
`change_canonical_identity_email` and also preserves the ID. Both require a non-empty audited
actor. Wrong organization, inactive user, stale email, subject collision, and invalid record
states fail with bounded categories that contain no claim or token material.

## Task binding

The trusted TokenReview result must contain exactly one
`agents.apelogic.ai/canonical-user:<user-id>` group in addition to the existing service and
acting-user/owner groups. The Task request body rejects `canonicalUserId`, `actingUser`, and all
other unknown fields, so the caller cannot select another person.

New Task rows bind lifecycle ownership and idempotency to `(service, canonical owner ID)`. Display
email remains immutable historical metadata on the row. An existing Task therefore remains
accessible after an audited email rename, while a different canonical user in the same service is
denied. Legacy Task rows are marked `legacy_reconnect_required`; they are not adopted by matching
email.

Every newly created Task runtime also carries the server-derived

```text
spec.canonicalAuthority.schemaVersion = steward/canonical-authority-binding/v1
spec.canonicalAuthority.ownerUserId = <canonical owner ID>
spec.canonicalAuthority.actingUserId = <same ID, only when a person is acting>
```

The binding contains no email, Google subject, hosted domain, or raw assertion. The Task API
constructs it from the resolved identity; Task request bodies have no identity field. Direct REST
runtime requests carrying `canonicalAuthority` are rejected, and Kubernetes admission permits its
initial value only from a configured trusted Steward writer. The complete binding is immutable.
Legacy runtimes omit it and must reconnect or follow an explicitly reviewed migration; consumers
must not infer it from `principal`, `owner`, email, annotations, or generic `bindings`.

## Compatibility and dependent slices

Migration `0012_canonical_user_identity.sql` is append-only. It creates the canonical user,
external-subject, and audit ledgers. Existing Task rows remain nullable and explicitly marked for
reconnect; no migration infers identity from email.

The following integrations consume this foundation and must preserve its boundary:

- browser sessions validate Google OIDC and retain the canonical user ID, never an email key;
- envelope and approval ownership use that authenticated canonical ID;
- the GitHub exchange maps its reviewed actor to that same ID and emits the trusted canonical-user
  group; workflow inputs cannot override it;
- Mint reads the typed canonical authority from the live UID-bound AgentRuntime. User and
  `service_for_user` modes require `actingUserId == ownerUserId` and project that minimum stable
  person reference while retaining existing issuer, audience, subject, and runtime-binding checks.
  Pure-service mode has no acting-person claim even when the runtime records an accountable owner;
- provider brokers look up grants by canonical user ID and never by token issuer plus email.

Mint changes are intentionally isolated in the separately reviewed Mint trust-boundary change.
The shared types required by that change are `CanonicalUserId` and
`CanonicalAuthorityBinding`; runtime credentials should carry the acting ID's exact `as_str()`
value as a dedicated stable-person claim, not embed `CanonicalPrincipal`, email, Google `sub`,
hosted domain, or raw organization assertions. Missing, unknown-version, or internally
inconsistent authority fails closed for person-bound Mint modes.
