# Changelog

All notable changes to Steward are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.4] - 2026-09-24

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

- Corrected release-only shell lint failures in the registry mirror. The
  `v0.2.3` workflow stopped before publishing images, charts, bundles, or a
  GitHub release.

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

[Unreleased]: https://github.com/apelogic-ai/steward/compare/v0.2.4...HEAD
[0.2.4]: https://github.com/apelogic-ai/steward/compare/v0.2.3...v0.2.4
[0.2.3]: https://github.com/apelogic-ai/steward/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/apelogic-ai/steward/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/apelogic-ai/steward/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/apelogic-ai/steward/compare/v0.1.23...v0.2.0
