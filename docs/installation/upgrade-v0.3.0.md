# Upgrade to Steward v0.3.0

Steward v0.3.0 is the User Envelope catalog, browser control-plane, and
federated Task-identity release. Upgrade from the complete v0.2.6 release using
one immutable 0.3.0 handoff: chart, all component images, reference runtime,
provider bundle, platform preflight bundle, and product-compatibility contract
must come from the same release commit and verified digests. Use a separately
signed installation BOM for the exact cross-product coordinates tested with
that handoff.

## Before rollout

1. Back up PostgreSQL and record the current migration head.
2. Upgrade capability catalog configuration from v1 to v2. The 0.3.0 runtime,
   Helm schema, and preflight reject v1 rather than deriving `accessClass`.
3. Keep Task orchestration and federated Task identity in their staged/default
   modes for the first binary rollout.
4. Stop template authoring during the rolling upgrade. New catalog revisions
   are not dual-written to legacy role-keyed Envelope storage, so mixed-version
   template writes are unsupported.
5. Verify every configured default LLM smoke-template value explicitly: exact
   model, budget, TTL, revision, and eligible member roles. Leave the feature
   disabled when any value is unknown.
6. Prepare a short-lived bearer token for an authenticated Steward administrator
   and an exact HTTPS operator origin. Day-two CLI commands use
   `STEWARD_OPERATOR_API_URL` and `STEWARD_OPERATOR_TOKEN_FILE`; they no longer
   accept a caller-selected audit actor or require PostgreSQL credentials.
7. To enable template-free requests, configure the complete
   `config.apiserver.customEnvelopeSafetyCeiling`. It must name only capabilities
   in the v2 catalog and explicitly bound budget, runtime minutes, TTL, and runner
   platforms/resources. The default `null` setting rejects custom requests.
8. Browser administration now requires
   `config.apiserver.stewardRunRelease`, populated from the verified installation
   BOM with `manifestSchemaVersion`, `version`, `workflowRepository`,
   `workflowCommit`, `actionCommit`, and `governedJobContainerImage`. Install
   `steward-run` v0.7.0 or later; apiserver startup fails closed on a missing,
   malformed, mutable, or older handoff.
9. Before upgrading to 0.3.2 or later with Gateway API enabled, configure the
   complete seven-entry `web.httpRoute.apiPaths` list from the chart README;
   incomplete route sets are rejected instead of silently reaching the web frontend.

## Migration and activation

Install the exact 0.3.0 artifacts. Migrations 0040–0051 are additive and retain
existing request, approval, RBAC, Task, run, and Envelope history. Migration
0041 imports legacy role-keyed template authority once. Migration 0051 permits
template-free requests by making template ID and revision nullable only as a
pair; it does not synthesize or rewrite history.

After every apiserver and controller runs 0.3.0:

1. Verify browser session, administrator request queue, template catalog, run
   detail, typed stage/log data, and connection status.
2. Exercise the supported RBAC CLI against a reserved test identity, including
   idempotent grant/revoke and JSON `effective-access` output.
3. Publish or apply an exact template revision and verify the configured
   automatic threshold: at-threshold auto-provisions, within-ceiling excess
   remains pending, and above-ceiling authority is rejected.
4. Provision two different templates with different content for one user.
   Submit one digest-qualified Task for each and verify an unqualified Task
   returns conflict while both remain active.
5. Submit and explicitly decide one template-free custom request, then tighten
   the safety ceiling and verify an already-pending request outside the new
   boundary can no longer be approved.
6. Only then activate optional federated Task identity or active orchestration
   modes according to their dedicated guides.

## Rollback boundary

Disable optional v3 Task identity and stop new writers before rollback. A
pre-0.3 binary cannot safely interpret template-free request rows, multiple
active Envelopes, or new catalog-only template revisions. Rollback to 0.2.6 is
therefore supported only when all of the following are proven:

- no template-free request exists;
- every user has at most one active provisioned Envelope;
- no template revision was authored after the upgrade;
- no v3 federated-subject operation must be served by the old binary; and
- migrations and all append-only history remain in place.

If any condition is false, roll forward with 0.3.0. Never delete or rewrite
request, RBAC, identity, decision, Task, run, or Envelope history to manufacture
rollback compatibility.
