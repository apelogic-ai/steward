# User Envelope and RBAC administration

Status: current release contract for Steward v0.3.0.

This guide covers the catalog-backed User Envelope model and the supported
day-two operator commands introduced after Steward 0.2.6. Canonical user IDs,
not email addresses, are authorization and mutation keys. Email is display-only.

## Authority model

`envelope_template_revisions` is the sole authority for template identity,
immutable revisions, eligible member roles, ceilings, and automatic-provisioning
thresholds. Legacy `envelopes(scope_kind = 'member_role')` rows remain read-only
for one compatibility window; new template and provisioning operations neither
read nor write them.

An administrator does not implicitly receive a member role. A user is eligible
for a template when at least one current member-role assignment intersects the
template's `memberRoles`. Role names are operator-defined and carry no built-in
semantics.

Template-backed requests are handled as follows:

| Requested authority | Result |
|---|---|
| At or below `autoProvisionThreshold` | Provision automatically |
| Above the threshold but within the ceiling | Remain pending for an administrator |
| Above the ceiling | Reject with `422` |
| Threshold omitted | Treat the ceiling as the threshold |

Template authoring rejects a malformed threshold or one outside the ceiling.
A template-free request omits both `templateId` and `templateRevision`, carries
the complete requested Envelope, and always remains pending until an explicit
administrator decision. Operators must configure
`config.apiserver.customEnvelopeSafetyCeiling`; otherwise custom requests fail
closed with `422`. Steward separately validates structure, capability-catalog
membership, and that budget, runtime minutes, TTL, runner platforms/resources,
models, and tools are within the current safety ceiling. The same checks run
again at approval, so a ceiling tightened while a request is pending fences it.

## Multiple active Envelopes and Task selection

One user may retain active Envelopes from different templates when both their
template identities and content digests differ. Provisioning the same template
and digest returns the existing active Envelope. A changed digest replaces only
the active Envelope from that template. Reusing the same digest through a
different template is a `409` conflict.

Task submissions may include the typed public selector:

```json
{"envelopeDigest":"steward:sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"}
```

Steward resolves the digest only among active Envelopes owned by the authenticated
canonical user. It is an integrity identity, not a credential. Omitting it remains
compatible when exactly one active Envelope exists; zero matches are rejected and
multiple matches return `409`. Both versioned Workflow and direct-package v2
submissions support the selector.

## Administrator provisioning API

`POST /admin/api/v1/envelopes/provision` uses the browser administrator session,
same-origin and CSRF boundary. Its JSON body contains the exact target and catalog
revision:

```json
{
  "ownerUserId": "usr_0123456789abcdef0123456789abcdef",
  "templateId": "default",
  "templateRevision": 1,
  "requestedEnvelope": {
    "revision": 1,
    "spec": {
      "llms": [{"provider": "provider-a", "model": "model-a"}],
      "tools": [],
      "budget": {"monthlyLimit": "1.00", "currency": "USD"},
      "ttl": "15m",
      "runner": {}
    }
  },
  "idempotencyKey": "provision-default-usr-0123456789abcdef0123456789abcdef-v1"
}
```

The target must exist, be active, hold an eligible member role, and request
authority within the exact template ceiling. The immutable request records the
target as owner and the authenticated administrator as actor. Exact retries are
idempotent.

## Supported operator CLI

Day-two commands are clients of Steward's bearer-authenticated operator API; they
do not connect to PostgreSQL. Configure the exact HTTPS origin and file-backed,
short-lived operator token. The token must authenticate as an administrator, and
the server-derived identity is recorded as the audit actor. A private deployment
CA may be supplied without replacing the system trust store.

```sh
export STEWARD_OPERATOR_API_URL=https://steward.example.com
export STEWARD_OPERATOR_TOKEN_FILE=/run/secrets/steward/operator-token
# Optional: export STEWARD_OPERATOR_CA_FILE=/run/secrets/steward/ca.crt
```

```sh
steward rbac users list --output json
steward rbac users show --user-id usr_0123456789abcdef0123456789abcdef
steward rbac roles list

steward rbac grant admin \
  --user-id usr_0123456789abcdef0123456789abcdef
steward rbac revoke admin \
  --user-id usr_0123456789abcdef0123456789abcdef

steward rbac grant member-role \
  --user-id usr_0123456789abcdef0123456789abcdef \
  --role engineer
steward rbac revoke member-role \
  --user-id usr_0123456789abcdef0123456789abcdef \
  --role engineer

steward rbac effective-access \
  --user-id usr_0123456789abcdef0123456789abcdef \
  --output json
```

Grant and revoke are retry-safe. Each effective state change appends an audit
event; an exact retry observes the already-requested state without duplicating
authority. `effective-access` reports administrator status,
member roles, eligible templates, and active Envelope IDs, template references,
and public digests.

The stable exit classes are `0` success, `2` invalid command/input/configuration,
`3` missing target, `4` inactive or forbidden identity, `5` state or idempotency
conflict, and `6` unavailable API or dependency. Human-readable output is the
default; `--output json` provides machine-readable output without credentials.
`bootstrap-rbac` remains only as the compatibility bootstrap command.

The CLI consumes the versioned bearer-authenticated contracts under
`/admin/operator/v1`: user and role reads, RBAC mutation, effective-access
projection, exact/latest template reads, immutable template apply, and exact
Envelope provisioning. These endpoints use the same administrator bearer guard
as Steward's existing operator routes. It first performs Kubernetes TokenReview;
if that rejects otherwise valid credentials, a configured verified Identity Task
JWT may authenticate, and it receives administrator authority only when its
validated groups contain the configured Steward administrator group. `401` means
the credential is invalid, `403` means the authenticated identity lacks administrator authority,
`404` means the target is absent, `409` reports state/idempotency conflict,
`422` reports invalid input, and `503` reports an unavailable dependency.

## Template apply and explicit provisioning

`templates apply` accepts strict JSON or YAML, rejects duplicate keys and YAML
aliases, and never overwrites an existing revision with different content:

```sh
steward templates apply --file default-template.yaml

steward envelopes provision \
  --user-id usr_0123456789abcdef0123456789abcdef \
  --template-id default \
  --template-revision 1
```

The optional Helm value `config.apiserver.defaultLlmTemplate.enabled` seeds the
same catalog record at startup. It is disabled by default. When enabled, operators
must supply at least one eligible member role and one exact model from the deployed
capability catalog, a bounded budget, TTL, and revision. Tools remain empty and the
automatic threshold is fixed to the ceiling. The conventional ID `default` is
configurable and has no special application semantics. Reapplying identical
content succeeds; drift at the same revision is a startup conflict.

## Upgrade and rollback

Migration 0051 makes the request template ID and revision nullable as a pair and
makes the event's template-revision snapshot nullable for custom requests. The
existing foreign key continues to protect template-backed requests. Existing
requests, events, RBAC audit history, and provisioned Envelope evidence are not
rewritten.

Roll back only to a binary that understands every row written after upgrade.
In particular, a pre-0051 binary cannot read a template-free request. Before such
a rollback, stop new writers and prove no custom request rows exist; do not delete
or rewrite append-only history to manufacture compatibility. Legacy role-envelope
rows remain available only for the stated read-only compatibility window.
