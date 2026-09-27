# Changelog

All notable changes to Steward are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0] - 2026-09-27

### Added

- Added the HyperShell browser/API redesign: one typed administrator request
  queue with history and structured deltas; cumulative spend and runtime-minute
  escalation controls; Envelope usage; trigger provenance; run stages, steps,
  incremental logs, cancellation and GitHub re-run operations; phase facets;
  connection status; catalog metadata; and durable onboarding progress.
- Added exact administrator provisioning of a catalog-backed User Envelope for
  an eligible canonical user, plus template-free custom requests that remain
  pending until an explicit decision. The browser exposes both flows: users may
  submit a complete custom Envelope without choosing a template, and
  administrators may provision an exact template revision to a canonical user.
  Custom requests fail closed unless an
  operator configures `customEnvelopeSafetyCeiling`; Steward rechecks their
  capabilities, budget, runtime minutes, TTL, and runner authority against the
  current ceiling at both creation and approval.
- Added multiple active User Envelopes with per-owner template and digest
  uniqueness, and the owner-scoped `envelopeDigest` selector for both versioned
  Workflow and direct-package Task submissions. Callers that omit the selector
  remain compatible only when exactly one active Envelope exists.
- Added the supported operator surface for canonical-user inspection, local RBAC
  grant/revoke, effective-access inspection, strict JSON/YAML template apply,
  and exact template provisioning. Commands are clients of bearer-authenticated
  administrative contracts, record the server-verified operator as actor,
  support human or JSON output and stable exit-code classes, and preserve
  `bootstrap-rbac` for bootstrap compatibility.
- Added an optional, disabled-by-default Helm seed for a bounded LLM-only smoke
  template. Operators must configure an exact model, budget, TTL, revision, and
  eligible roles; tools remain empty and the automatic threshold equals the
  ceiling.
- Added unauthenticated OAuth protected-resource discovery and an explicitly
  enabled `steward-task-v3` contract keyed by the verified Identity issuer and
  stable numeric GitHub actor subject. The default remains v2-only.
- Added additive migration 0040 for idempotent federated-subject observation,
  conflict-safe association/replacement/disable, and append-only administrator
  audit. It performs no historical identity, Task, run, runtime, or Envelope
  backfill.
- Added browser-administrator APIs to inspect and manage observed federated
  subjects. First observation and association grant no User Envelope or Task
  authority; normal source and active User Envelope admission remain required.

### Changed

- Helm now accepts the documented `null` or omitted custom Envelope ceiling and
  steward-run release projection while browser administration is disabled.
  Enabling browser administration still requires a complete, schema-valid
  steward-run release projection.
- Replaced Steward's source-pinned cross-product deployment lock with an
  attested product-compatibility contract. It declares `steward.task/v2` and a
  `steward-run` v0.7.0 minimum for `envelopeDigest`; release/integration
  packaging now owns the separately signed installation BOM containing exact
  product, commit, image, and chart digests.
- Advanced the Steward release handoff to `steward.release-handoff/v2` for the
  product-compatibility reference. The registry mirror continues to accept v1
  handoffs for existing releases.
- Envelope Template revisions are now the sole authority for new template,
  request, approval, and provisioning operations. Legacy role-keyed Envelope
  rows remain read-only for one compatibility window; compatibility authoring
  routes adapt to the catalog and no longer dual-write.
- Template `autoProvisionThreshold` is enforced: requests at or below it
  provision automatically, requests above it but within the ceiling require
  review, and requests above the ceiling are rejected. A missing threshold
  retains the historical ceiling-as-threshold behavior.
- Decision filing now carries authoritative `template_id` and an optional real
  eligibility-role snapshot. Deprecated `member_role` remains optional during
  transition and is never populated with a template ID.
- Capability catalog schema v2 is now required and v1 configuration is rejected
  by runtime validation, Helm schema validation, and preflight. Upgrade the
  catalog before rolling out this release. New template writes are not
  dual-written to the legacy store, so mixed-version template authoring and
  rollback after a new catalog write are unsupported.
- The embedded migration head advances through `0051`. Migrations `0041`–`0051`
  add the template catalog, decision metadata, browser preferences, escalation
  and runtime-minute ledgers, typed stage and live-log storage, re-run authority,
  durable onboarding acknowledgement, and template-free request shape. They are
  append-only; a Helm rollback does not reverse them, and the v0.3 upgrade guide
  defines the resulting rollback boundary.
- Valid v2 credentials may idempotently seed only their same verified
  issuer/subject and already-resolved canonical user as a best-effort transition
  to v3. Seeding failure, conflict, or disablement never changes v2
  authentication or admission.
- Rollback now requires disabling v3 before returning to a v2-only binary and
  preserving migration 0040 data; the federated identity upgrade guide records
  the backup, activation, verification, and rollback sequence.

## [0.2.6] - 2026-09-24

### Fixed

- Removed the newly introduced release-only Codex/OpenShell runtime gate because it exercised a
  different supervisor topology from the established runtime E2E path and blocked publication
  without providing representative deployment evidence. Codex image build, vulnerability
  scanning, SBOM, provenance, and publication remain required.

## [0.2.5] - 2026-09-24

### Added

- Added one attested governed-platform compatibility manifest with exact Steward,
  companion-product, OpenShell, agent-sandbox, SPIRE, MCP-GW, and LiteLLM
  coordinates and contracts.
- Added the named `steward.connections.github/v1` and
  `steward.connections.github/v2` MCP-GW authority selectors with a compatible
  migration path from the deprecated version-like selector.
- Added explicit SPIRE identity/upgrade requirements and LiteLLM Responses and
  Anthropic Messages URL/model semantics to the installation contract.
- Added a real Envoy Gateway backend-TLS E2E using an isolated Kind cluster,
  plus public-CA rotation guidance for the apiserver `BackendTLSPolicy`.

### Changed

- Replaced the Docker/buildx registry helper with a released daemonless ORAS
  image-and-chart mirror supporting explicit mappings, no-write planning,
  collision-safe resume, deterministic evidence, and Flux OCI digest output.
- Corrected the browser-administration bootstrap procedure to run the installed
  `/usr/local/bin/steward bootstrap-rbac` command in the apiserver Deployment.

### Fixed

- Kept registry-mirror progress output separate from its machine-readable JSONL
  result. The `v0.2.4` workflow stopped during live mirror conformance and the
  Codex image scan; it did not publish a GitHub release or complete handoff.
- Updated the Codex reference runtime's inherited kernel headers and Node tar
  packages, recorded narrowly scoped VEX evidence for kernel implementations
  absent from the image, and added the complete native `linux/amd64` image scan
  to pull-request CI before a release tag can be cut.

## [0.2.4] - 2026-09-24

The release workflow stopped during live registry-mirror conformance. It did
not publish a GitHub release or complete release handoff; OCI artifacts created
by the incomplete workflow are not a complete Steward release.

## [0.2.3] - 2026-09-24

The release workflow stopped during validation and published no artifacts.

## [0.2.2] - 2026-09-24

### Added

- Added optional Secret- or ConfigMap-backed PostgreSQL CA projection for API server and
  controller connections using `sslmode=verify-full`.
- Added a released, standalone `linux/amd64` provider-profile validator and installer with
  deterministic output and machine-readable bundle evidence.
- Added a released registry mirror tool that preserves OCI indexes, verifies copied target
  digests and platforms, and emits a deterministic credential-free deployment lock.
- Added a supported `linux/amd64` Codex 0.140.0 reference runtime with an immutable release digest,
  SBOM, provenance, provider-profile conformance, and mirroring instructions.
- Added a deterministic platform preflight bundle that generates Helm/Flux values from one
  non-secret input and validates cross-component deployment relationships before rollout.
- Added joint public-hostname, certificate-name, and live read-only Gateway validation with
  exact one-label wildcard semantics.
- Gateway API HTTPRoute deployments now render a fail-closed `BackendTLSPolicy` for the
  apiserver HTTPS Service, with explicit public-CA trust and full Service-DNS SNI inputs.
- Added one explicit namespace-map schema with compact and separated layouts and deterministic
  namespace-qualified reference generation.
- Added read-only EKS VPC CNI detection and a bounded, run-owned NetworkPolicy deny/allow
  enforcement smoke that fails closed when enforcement cannot be proven.

### Fixed

- Reconciled externally submitted Task runtimes only from their exact persisted Task authority,
  including successful-completion finalization, Task-owned TTL, and cleanup ordering before
  Kubernetes deletion.
- Rejected all-zero placeholder digests for required component, execution-binding, provider-profile,
  stable-bridge, and Connections-bridge images before deployment or runtime provisioning.

## [0.2.1] - 2026-09-23

### Fixed

- Isolated release-version fixtures from the tag environment so the validated v0.2 product
  artifacts can be published. The failed v0.2.0 release produced no images, chart, bundle, or
  GitHub release.

## [0.2.0] - 2026-09-23

### Removed

- Removed Service Envelope APIs and all live Service Envelope admission and controller authority.
- Removed route-scoped Service Envelope bootstrap authorization and configuration.
- Removed the legacy no-User-Envelope workflow catalog, adopted-runtime submission, and
  copy-smoke bootstrap path.
- Removed Service Envelope fields and terminology from current browser Run and template views.

### Added

- Added a bounded, deployment-owned, non-authoritative capability catalog for administrator model
  and tool selection.
- Added immutable User Envelope snapshots as the sole controller recovery authority for external
  Tasks.
- Added explicit upgrade validation for historical Service Envelope-era Tasks.
- Added a machine-readable release handoff alongside the human-readable artifact handoff.

### Changed

- User Envelope is now the sole authority for externally submitted Tasks.
- New orchestration records require either exact User Envelope authority or exact code-owned
  internal authority pins.
- Identity integration expects the Task-only policy contract introduced by Identity v0.5.0.
- Direct Git package and immutable versioned Workflow submissions are the supported governed Task
  paths.

### Security

- Removed a dormant separately scoped bootstrap authority.
- Eliminated the second mutable global Task authority.
- Prevented execution without a complete pinned user or internal authority.
- Preserved fail-closed revalidation when pinned User Envelope authority is revoked or becomes
  inactive before execution.

### Migration

- Unfinished legacy Tasks without exact authority must be finalized before upgrade.
- Migration `0039_user_envelope_only_task_authority.sql` upgrades recoverable unfinished Tasks to
  orchestration v3 and preserves terminal history unchanged.
- Rollback after migration requires restoring both the pre-upgrade database and v0.1.23 desired
  state; image-only rollback is unsupported.
- Service Envelope bootstrap configuration must be removed and the descriptive capability catalog
  configured when browser administration is enabled.

Earlier releases are available on the [GitHub releases page](https://github.com/apelogic-ai/steward/releases).

[Unreleased]: https://github.com/apelogic-ai/steward/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/apelogic-ai/steward/compare/v0.2.6...v0.3.0
[0.2.6]: https://github.com/apelogic-ai/steward/compare/v0.2.5...v0.2.6
[0.2.5]: https://github.com/apelogic-ai/steward/compare/v0.2.4...v0.2.5
[0.2.4]: https://github.com/apelogic-ai/steward/compare/v0.2.3...v0.2.4
[0.2.3]: https://github.com/apelogic-ai/steward/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/apelogic-ai/steward/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/apelogic-ai/steward/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/apelogic-ai/steward/compare/v0.1.23...v0.2.0
