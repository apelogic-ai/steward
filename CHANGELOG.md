# Changelog

All notable changes to Steward are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/apelogic-ai/steward/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/apelogic-ai/steward/compare/v0.1.23...v0.2.0
