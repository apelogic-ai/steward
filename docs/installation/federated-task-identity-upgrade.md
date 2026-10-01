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
   or controller applies migrations `0040_federated_subject_identity.sql` and
   `0053_connection_verified_federated_subjects.sql`. Migration 0040 creates the
   federated-subject and audit tables. Migration 0053 adds association methods
   and bounded connection-verification evidence; it classifies existing
   associations without changing their canonical-user bindings or prior audit
   rows. Neither migration infers an association from login, display name, or
   email or rewrites historical Tasks, runs, runtimes, or Envelopes.
2. Verify migrations 0040 and 0053 completed, including the 0053 association
   method and connection-evidence columns and constraints, and confirm existing
   v2 submission, retry, source provenance, and User Envelope admission still
   work.
3. Configure the exact public Steward origin and enable v3:

   ```yaml
   taskIdentity:
     enabled: true
     issuer: https://identity.example.test
     audience: steward-task-api
     resource: https://steward.example.test
     federatedSubjects:
       enabled: true
       autoAssociateFromConnections: true
     publicJwksConfigMap:
       name: steward-task-identity-jwks
       key: jwks.json
   ```

4. Verify `GET /.well-known/oauth-protected-resource` returns the exact
   resource and issuer, `Cache-Control: public, max-age=300`, bearer-header
   support, and both `steward-task-v2` and `steward-task-v3`.
5. Sign in as a new user and connect the same GitHub account that will trigger
   the workflow. After connection status reports its numeric GitHub account ID,
   verify Steward records `github-actions:actor:<id>` as associated with that
   canonical user and audits `connection_verified` with provider `github` and
   the exact numeric ID.
6. Submit the first valid v3 credential for that actor. Normal source
   authorization and active User Envelope admission must pass without an
   administrator association or a retry. Association does not grant either
   repository access or runtime authority.
7. Set `autoAssociateFromConnections=false` to verify the manual-review mode:
   an unobserved actor returns `403 task_identity_unassociated` until an
   administrator associates the observed subject. Re-enable the default after
   the check unless manual review is the intended policy.
8. Prove wrong issuer, audience, signature, algorithm, key ID, expired or future
   time, malformed actor subject, caller-supplied canonical identity, disabled
   subject, and a canonical user without a matching active Envelope all fail
   before Task reservation.

## Rolling compatibility

Migrations 0040 and 0053 are additive, so old v2-only binaries ignore their
tables and columns. New
binaries accept v2 throughout the rollout. Do not enable v3 until every
apiserver that can receive Task traffic supports it; otherwise requests routed
to an older pod will fail inconsistently. The database remains the only source
of federated-subject association state.

## Rollback

Before rolling back binaries, set
`taskIdentity.federatedSubjects.enabled=false` and verify discovery advertises
v2 only. Stop new submissions and drain or fence in-flight work under the
ordinary Task rollback procedure. Previous v2-capable binaries may then run
against the additive migration 0040 and 0053 schema, but the federated-subject tables
and their audit history must remain intact.

Do not reverse migrations 0040 or 0053, delete observed subjects, or restore a database
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
