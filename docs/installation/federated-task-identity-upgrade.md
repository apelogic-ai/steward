# Federated Task identity upgrade and rollback

This guide enables the additive `steward-task-v3` identity contract. Existing
`steward-task-v2` tokens, Tasks, runs, canonical authority bindings, User
Envelopes, and in-flight retries remain on their existing path. The default
chart configuration continues to accept v2 only.

## Before the upgrade

1. Stop new Task submissions and record the exact chart and image digests,
   values checksum, Helm revision, and current database migration version.
2. Take a consistent encrypted PostgreSQL backup and prove that it restores to
   a separate target.
3. Confirm the configured Identity issuer can issue the exact
   `steward-task-v3` contract with subject
   `github-actions:actor:<positive-numeric-id>`, audience
   `steward-task-api`, ES256 signature, bounded `kid` and `jti`, current time
   claims, and the existing signed source provenance.
4. Keep `taskIdentity.federatedSubjects.enabled=false` during the binary and
   migration rollout.

## Upgrade and activate

1. Upgrade all Steward components to the same immutable release. The apiserver
   or controller applies migration `0040_federated_subject_identity.sql`.
   Migration 0040 creates new federated-subject and audit tables only. It does
   not update, backfill, or infer any historical identity, Task, run, runtime,
   Envelope, or audit row.
2. Verify migration 0040 completed and existing v2 submission, retry, source
   provenance, and User Envelope admission still work.
3. Configure the exact public Steward origin and enable v3:

   ```yaml
   taskIdentity:
     enabled: true
     issuer: https://identity.example.test
     audience: steward-task-api
     resource: https://steward.example.test
     federatedSubjects:
       enabled: true
     publicJwksConfigMap:
       name: steward-task-identity-jwks
       key: jwks.json
   ```

4. Verify `GET /.well-known/oauth-protected-resource` returns the exact
   resource and issuer, `Cache-Control: public, max-age=300`, bearer-header
   support, and both `steward-task-v2` and `steward-task-v3`.
5. Submit one valid v3 credential for an unobserved actor. Expect
   `403 task_identity_unassociated`, one observed subject, one `observed` audit
   event, and no Task or Envelope creation.
6. Through the browser administrator API, associate that subject with an
   existing active canonical user using its current revision. Verify the audit
   event and then submit again. Normal source authorization and active User
   Envelope admission must still pass; association does not grant either.
7. Prove wrong issuer, audience, signature, algorithm, key ID, expired or future
   time, malformed actor subject, caller-supplied canonical identity, disabled
   subject, and a canonical user without a matching active Envelope all fail
   before Task reservation.

## Rolling compatibility

Migration 0040 is additive, so old v2-only binaries ignore its tables. New
binaries accept v2 throughout the rollout. Do not enable v3 until every
apiserver that can receive Task traffic supports it; otherwise requests routed
to an older pod will fail inconsistently. The database remains the only source
of federated-subject association state.

## Rollback

Before rolling back binaries, set
`taskIdentity.federatedSubjects.enabled=false` and verify discovery advertises
v2 only. Stop new submissions and drain or fence in-flight work under the
ordinary Task rollback procedure. Previous v2-capable binaries may then run
against the additive migration 0040 schema, but the federated-subject tables
and their audit history must remain intact.

Do not reverse migration 0040, delete observed subjects, or restore a database
backup merely to remove v3. If rollback requires a pre-0040 database, restore
the complete pre-upgrade backup to a separate target and switch all compatible
components together; that discards every post-backup write, not only federated
identity data.

## Documentation inventory

The implementation change updates these current contracts:

- root `README.md`: default v2 and opt-in v3 summary;
- `charts/steward/README.md`, `values.yaml`, and `values.schema.json`: exact
  activation values and defaults;
- `docs/task-submission-api.md`: discovery, token contracts, errors, authority,
  and administrator endpoints;
- `docs/canonical-user-identity-v1.md`: exact subject association and canonical
  resolution;
- `docs/admin-ui-contract-v1.md`: administrator API and mutation boundary;
- `docs/installation/installation-guide.md` and
  `docs/installation/platform-deployment-order.md`: install and activation
  order;
- `migrations/README.md`: additive schema and rollback boundary;
- `CHANGELOG.md`: release-facing behavior and operational data impact; and
- this guide: backup, rollout, verification, compatibility, and rollback.

`docs/solution-overview.md`, frozen `docs/contracts/m1/v1/`, and version-scoped
historical delivery/roadmap documents do not define the current Task-token wire
contract and remain historically unchanged. `docs/README.md` links this guide
from the current documentation index.
