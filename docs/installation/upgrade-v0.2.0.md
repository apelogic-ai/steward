# Upgrade to Steward v0.2.3

Status: current release contract for upgrades from v0.1.23.

Steward v0.2 removes the Service Envelope as live Task authority. External Tasks use only an
exact provisioned User Envelope; internal product operations use exact code-owned authority pins.
Migration `0039_user_envelope_only_task_authority.sql` establishes orchestration version 3.

## Preconditions

1. Back up PostgreSQL using the database operator's tested procedure.
2. Put Task orchestration in `staged` mode and stop every v0.1.23 writer.
3. Inspect every unfinished v0.1.23 Task.
4. Finalize each unfinished legacy Task that lacks either a complete User Envelope pin or a
   complete internal-authority pin. Do not construct or infer authority for it.
5. Verify that each unfinished direct/versioned user Task has its exact Envelope instance ID,
   revision, digest, canonical owner, and a matching provisioned request event containing the
   approved snapshot.
6. Verify that each unfinished internal Task has its complete authority ID, version, and digest.
7. Remove obsolete deployment inputs for the unversioned workflow catalog and route-scoped
   bootstrap workflow. Configure `config.apiserver.capabilityCatalog` with descriptive models and
   tools only.

The migration fails before changing the authority contract if any unfinished Task is ambiguous.
It also fails if an exact approved User Envelope snapshot cannot be recovered.

## Upgrade

1. Install the exact v0.2.3 chart and image digests from the release handoff with orchestration
   still `staged`.
2. Allow the apiserver or controller to apply the append-only migration set through `0039`.
3. Verify that unfinished user Tasks are version 3 with `authority_kind=user-envelope`, their
   exact approved snapshot, and no live global-authority digest.
4. Verify that unfinished internal Tasks are version 3 with `authority_kind=internal` and their
   exact ID/version/digest.
5. Verify that terminal v1/v2 history remains readable and cannot be resumed.
6. Verify the capability API and template editor, then activate orchestration in a separate
   rollout.
7. Submit one bounded user Task and prove that its Run detail shows the exact User Envelope
   evidence and no global authority.

## Rollback boundary

- **Before migration:** restoring the v0.1.23 workloads is safe if no v0.2 desired state was
  applied.
- **After migration, before any v0.2-only Task is admitted:** image-only rollback is not supported.
  Restore the pre-upgrade database backup together with all v0.1.23 workloads and configuration.
- **After a v0.2 Task exists:** v0.1.23 cannot interpret orchestration v3 authority. Complete or
  explicitly retire v0.2 work under a reviewed recovery procedure, then restore both database and
  desired state. Do not point v0.1.23 binaries at the migrated live database.

Database rollback and desired-state rollback are one operation. A Helm rollback does not reverse
SQL migrations.

## Historical records

Terminal Service Envelope-era rows remain immutable audit history. They are not displayed as
current authority, cannot admit new work, and cannot be used to restart or widen a Task.
