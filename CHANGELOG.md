# Changelog

All notable changes to Steward are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- Get started step 4 now publishes inline test runs submitted by Steward v0.3.8 or
  earlier, whose tested package is a root `task-definition.json` with a path-backed
  `prompt.md`. Both files are published unchanged with the generated `package-path`
  caller, and the published package digest equals the tested closure digest. The
  governed publication allowlist accepts a root `task-definition.json` and one
  `prompt.md` beside the Task definition. Step 4 previews every package file.
- Evidence that cannot be published now returns `409` with
  `error: tested_package_unpublishable` and a bounded `reason`
  (`evidence_unavailable`, `source_not_inline`, `package_files_invalid`,
  `closure_mismatch`, `package_shape_unsupported`, `envelope_unavailable`, or
  `workflow_unavailable`) instead of an empty `409`; Get started shows that reason.

## [0.3.11] - 2026-10-06

### Added

- This complete replacement includes every Added and Fixed item recorded under
  the failed, unpublished v0.3.10 entry and the incomplete v0.3.9 entry below.

### Fixed

- Removed the undeclared ripgrep dependency from release-path shell scripts;
  their exact and regular-expression assertions now use portable `grep` modes.
- Made pull-request quality validation and tagged-release validation run the
  same `cargo xtask ci` entrypoint on Ubuntu 24.04, with fail-closed tool
  preflight before any external command is executed.
- Added pull-request release-candidate validation for the same source-chart
  profiles, locally built component images, critical vulnerability policy, and
  clean-cluster installation exercised by the tagged release.
- The v0.3.10 tag failed during repository validation before any images, chart,
  assets, or GitHub release were published; it must not be used.

## [0.3.10] - 2026-10-06

### Added

- This complete replacement includes every Added and Fixed item recorded under
  the incomplete v0.3.9 entry below.

### Fixed

- Kept the deployment-configurable starter Task optional in the Helm schema so
  the core installation profile can use the apiserver's built-in default. The
  release gate now lints this exact unset profile before publication.
- The v0.3.9 chart and component images were published, but mandatory clean-
  cluster acceptance failed before the GitHub release handoff was created;
  those incomplete artifacts must not be used.

## [0.3.9] - 2026-10-06

### Added

- Added same-repository direct Task submission by `packagePath`, with request-level
  execution-log diagnostics and immutable implicit-invocation evidence. The
  protected-resource metadata now advertises `steward_package_path_supported` so
  steward-run 0.8.0 or later can gate this path. Existing invocation manifests
  remain the cross-repository mechanism.
- Added `promptText` as a bounded inline alternative to a prompt file in
  `steward.task-definition/v2`. Save this task now produces the two-file repository
  path with `steward-run` 0.8.0 or later and falls back to the compatible invocation
  manifest for older reviewed releases. These fields require Steward 0.3.9 or later;
  earlier Steward releases reject them.
- Added a deployment-configurable starter Task, served at runtime to the browser with
  configurable package path, inputs, execution-log default, presentation fields and
  source examples. The built-in Hello World prompt now explicitly permits the shell
  write it requires while prohibiting network and MCP access.
- Added the seven-step Get started journey from GitHub connection and first Envelope
  through a governed browser test, exact package publication, workflow verification,
  GitHub dispatch and linked result. Progress and navigation are restored from
  persisted server evidence without running governed GitHub probes on page load.
- Added governed GitHub repository automation that discovers an owner-scoped target,
  publishes an exact Task package and pinned caller workflow through a pull request,
  observes merge readiness, dispatches the workflow, and reports the resulting run.
- Added an immutable Task detail page addressed by exact package digest, with package
  files, metadata, run history, repeat execution, and GitHub publication controls.
- Added an owner-scoped Task library with immutable content-addressed versions, role
  sharing, exact historical links, and one Tasks/Run now UI for authored, exact Git,
  and published Workflow sources. Additive migration 0067 stores drafts and versions.

### Fixed

- Kept new Task output archives limited to declared `out/` files while retaining
  stdout and stderr only in the existing execution-log transcript fields. Additive
  migration 0066 records the archive contract without rewriting historical rows.
- Bounded terminal run-event history by reconnect grace and inactive-task LRU capacity
  while preserving `Last-Event-ID` resume for active streams.
- Preserved published Workflow Tasks after a browser run, mapped duplicate owner Task
  names to HTTP 409, and aligned Task sharing with Steward's canonical role grammar.

## [0.3.8] - 2026-10-05

### Added

- This complete replacement includes every Added, Fixed, and Security item,
  plus every upgrade note, recorded under the incomplete v0.3.7 and v0.3.6
  entries below.

### Fixed

- Supplied the required SPIRE controller class to the released-artifact core
  installation harness and added a pre-tag contract regression for that exact
  value. The v0.3.7 chart and images were published, but its mandatory clean-
  cluster acceptance failed before the GitHub release handoff was created;
  those incomplete artifacts must not be used.

## [0.3.7] - 2026-10-05

### Added

- This complete replacement includes every Added and Fixed item, plus every
  upgrade note, recorded under the incomplete v0.3.6 entry below.

### Fixed

- Replaced run-detail fetch polling with one held, owner-scoped SSE connection
  that sends an atomic run-and-timeline snapshot, incremental state events and
  heartbeats, resumes with `Last-Event-ID`, enforces a per-user stream cap, and
  closes after terminal finalization. Run headers and job states now derive
  from the same snapshot, including promotion to Running once a runtime binds.
- Kept outputs and the inline-task save panel pending until successful
  finalization, returned `409 outputs_pending` before then, and removed the
  false agent-compatibility warning when Run now selects a compatible fallback.
- Kept reserved `.steward/diagnostics/stdout.log` and `stderr.log` transcripts
  out of browser output listings and downloads while continuing to reject every
  other regular file outside `out/`. Runs with full execution-log capture can
  now list and download their declared output files after success.

### Security

- Updated the Steward web application from Next.js 16.3.3 to 16.3.6 to
  remediate `GHSA-vcvr-r3jv-pc5j`, a critical remote-code-execution advisory
  affecting `next/og` `ImageResponse`. The v0.3.6 publication is incomplete and
  its artifacts must not be used. The v0.3.7 publication later failed mandatory
  released-artifact acceptance and is also incomplete; v0.3.8 is the complete
  replacement.

## [0.3.6] - 2026-10-04

### Added

- Added browser **Run now** for inline v2 packages, exact Git package locators,
  and published Workflow aliases. Browser Tasks use the authenticated canonical
  user and selected User Envelope, record immutable source and authority evidence,
  accept bounded JSON inputs, enforce four active browser Tasks per canonical user,
  and expose owner-scoped `out/` downloads. Additive
  migration 0057 records Task origin and the per-template inline-authoring switch.
- Started GitHub connection authorization asynchronously: the browser mutation
  now returns an owner-scoped operation identifier and polling deadline with
  HTTP 202, and the UI polls that operation until the one-time authorization
  URL or a bounded terminal failure is available. A short observation grace
  lets the UI read Steward's durable deadline result, while a reused terminal
  operation always receives an immediate observation window. A retry that
  reuses a completed start remains pollable until its pending OAuth flow expires
  rather than inheriting the elapsed runtime-response deadline.
- Added an administrator Get started workspace backed by live, read-only setup
  checks for orchestration bindings, administrator-scoped GitHub Connect,
  capability-catalog coverage, member-ready templates, active members, and
  optional GitHub Actions automation. Automation readiness uses only durable
  GitHub-ratified Task provenance persisted for both versioned workflows and
  direct packages, and `githubSource` is required only after direct-package use.
  The guide refreshes after each read settles without an apiserver restart and
  can be hidden or restored per browser. Migration 0056 backfills direct-package
  provenance; historical versioned Workflow rows remain unknown because their
  discarded identity evidence cannot be reconstructed safely.
- Added authoritative optional toolset metadata to the deployment capability catalog and a
  searchable template tool picker with read-only bulk selection, per-tool confirmation for
  write/destructive grants, access-class counts, and exact revision diffs. Existing v2 catalog
  tools without toolsets remain valid and use an ungrouped fallback.
- Added opt-in MCP-GW connection-status v2 consumption and default-on GitHub
  Connect association for `steward-task-v3`, matching only the immutable numeric
  GitHub account ID and recording connection-verification evidence in additive
  migration 0053. Deployments can retain manual association with
  `taskIdentity.federatedSubjects.autoAssociateFromConnections=false`.
- Added explicit remote and vendored installation modes for generated
  `steward-run` callers. Vendored mode requires a verified steward-run v0.7.6 or
  later workflow asset at the fixed repository-local path, while task-auth
  discovery removes obsolete identity-exchange and CA inputs from generated
  callers.
- Added browser-first member administration: administrators can invite verified
  organization email addresses before first sign-in, assign member roles and
  administrator access, inspect member and identity details, unlink or disable
  federated identities, disable and re-enable members, revoke pending
  invitations, and preserve last-administrator protection. Unassigned users see
  an explicit access-pending page. Additive migrations 0060-0062 preserve the
  member, display, sign-in, invitation, and lifecycle audit state.
- Made the four-step browser hello-world journey the primary user onboarding
  path, with GitHub Actions as an optional fifth step. The dismissible Get
  started banner is shared across user pages and can be restored from Settings.
- Added live Run detail updates over owner-scoped SSE with bounded polling
  fallback, replay of browser-origin runs through normal admission, job and log
  deep links, explicit empty/unavailable log states, and opt-in full execution
  log capture.

### Fixed

- Bound the governed Mint and stock OpenShell v0.0.98 sandbox SPIFFE
  registrations to the operator-selected SPIRE controller class, generated the
  sandbox registration from the chart, and changed the required governed
  Connections lane to use the published OpenShell supervisor in the supported
  sidecar topology instead of a local patch.
- Fail every governed Task promptly when its exact current runtime reports a
  terminal start failure, and refused to adopt a same-name runtime whose UID
  differs from the Task's durable binding.
- Distinguished the deliberate staged-orchestration boundary from a Connections
  service outage. Governed connection mutations now return
  `connections.orchestration_not_active`, the browser explains that activation
  is required, and startup logs the staged bridge state once.
- Failed governed connection starts promptly when their exact runtime reports a
  terminal start failure, and preserved distinct runtime-authentication,
  token-grant, provider-response, gateway, runtime-create, runtime-start, and
  deadline categories through audit records, API problem bodies, and actionable
  browser messages. Disconnect now follows the same asynchronous owner-scoped
  operation polling contract as connection start, avoiding edge request
  timeouts while preserving the durable terminal result.
- Published the immutable `repo-summary@2` onboarding Workflow with an explicit
  `out/summary.md` result and migrated existing revision-1 installations without
  rewriting their history. The Workflow authoring UI now states that governed
  results must be written beneath `out/`.
- Made the deprecated steward-run governed job image optional in the chart,
  platform preflight, and versioned GitHub Actions generator. Existing pinned
  image values remain accepted for the lower-level v1 smoke renderer.
- Preserved bounded MCP-GW HTTP status and safe reason diagnostics for failed governed
  connection operations in additive migration 0052 and returned them through the
  connection API's `upstreamStatus` and `detail` fields.
- Distinguished an authenticated `steward-task-v2` credential that names an
  unknown canonical user (`403 task_identity_unknown_user`) from an invalid
  credential (`401`), and documented Identity v6 / `steward-task-v3` as the
  recommended new-install contract while retaining v5 / v2 compatibility.
- Returned specific, actionable failures when exact Git source resolution is
  disabled, when task orchestration remains staged, and when a template omits
  complete role, model, tool, budget, TTL, or runner authority instead of
  collapsing those cases into generic dependency or form errors.
- Preserved finalized-Task monotonicity while migrations 0056 and 0057 backfill
  provenance: additive migration 0054 opens only the three required fields and
  migration 0058 restores the strict trigger. Migration 0059 admits the distinct
  browser direct-package pin shape without weakening existing Workflow pins.
- Persisted the exact successful inline package used by browser Run now, admitted
  packages under runtime-minute ceilings, surfaced bounded persistence failures,
  and proved that the repository handoff and subsequent GitHub Actions run retain
  the browser run's exact package content digest.
- Prevented browser repository submissions from revealing whether an unlisted or
  inaccessible private repository exists; both cases return the same authorization
  response before Task reservation.
- Preserved MCP-GW's bounded machine-readable failure code alongside safe status
  and reason diagnostics in additive migration 0063, and hardened provider-control
  transcript handling without storing raw gateway responses.

### Upgrade notes

- Existing governed installations must set `spire.className` before upgrading.
  List the classes already used by the installed SPIRE controller with
  `kubectl get clusterspiffeids.spire.spiffe.io -o custom-columns=NAME:.metadata.name,CLASS:.spec.className`,
  then put the selected class in both the platform-preflight input and the
  Steward chart values. Governed chart validation now fails closed when the
  value is empty.
- The chart now enables `spire.sandboxRegistration.enabled` by default and
  creates `ClusterSPIFFEID/steward-openshell-sandboxes`. Before upgrading, use
  exactly one owner for the OpenShell sandbox registration: remove the
  existing platform-owned registration before enabling the chart-owned one, or
  set `spire.sandboxRegistration.enabled=false` and verify that the retained
  registration has the same namespace selector, pod selector, annotation, and
  SPIFFE ID template documented in
  [`docs/installation/openshell-v0.0.98.md`](docs/installation/openshell-v0.0.98.md).
  Rerun platform preflight with the chosen `spire.className` and ownership
  setting before applying the upgrade.

## [0.3.5] - 2026-09-30

### Security

- Removed unrestricted Kubernetes user and group impersonation from the
  apiserver and controller ClusterRoles. AgentRuntime writes now use the exact
  Steward service-account identities and remain subject to webhook Envelope,
  grant, role-binding, principal-immutability, and authority checks.

### Upgrade notes

- `helm upgrade` removes the obsolete impersonation rules; no manual RBAC
  action is required. During a rolling upgrade, an old apiserver may briefly
  receive `403` responses after the ClusterRole changes, and a new apiserver
  served by the old controller webhook may have noncanonical user-runtime
  writes rejected until both components have rolled. Retry after the apiserver
  and controller converge on the same version. Operators who added separate
  impersonation grants for these service accounts may remove them.

## [0.3.4] - 2026-09-29

### Changed

- Published provider-profile bundle 1.2.2 with the complete bounded MCP
  transport method set, including `DELETE` for session close, and retained an
  exact 1.2.1-to-1.2.2 upgrade transition.
- Expanded platform preflight validation for cluster-local LiteLLM management
  URLs, optional ARC installations, Workflow runtime namespaces, custom Mint
  audiences, and OpenShell-blocked IPv6 and IPv4-mapped CIDRs.
- Documented the tested MCP-GW 0.5.1, LiteLLM v1.93.0, SPIRE, edge-timeout,
  resource, Pod Security, storage, pricing, and provider-consumer contracts.
- Restored the HyperShell public identity, logo, favicon, page titles, and
  product-facing copy in the web UI while retaining `steward` for package,
  chart, CLI, and API names.

### Fixed

- Allowed Codex-only generated configuration to omit the Anthropic inference
  endpoint while retaining fail-closed validation when a Claude binding is
  active.
- Kept apiserver startup available when the immutable onboarding Workflow names
  an execution binding not installed in the current deployment.
- Corrected provider-profile upgrade validation for deployment-specific Mint
  audiences and required MCP session-close transport.

### Upgrade from 0.3.3

This patch adds no database migration and changes no default runtime class.
Before upgrading an execution-enabled installation, stop the controller and
apiserver, enable OpenShell `providers_v2_enabled`, and apply the exact
provider-profile bundle 1.2.1-to-1.2.2 transition. Rerun platform preflight,
use the regenerated profile digests and Helm values, then restart both
consumers. The temporary v0.3.3 requirement to populate
`config.apiserver.anthropicInferenceEndpoint` for a Codex-only binding is no
longer necessary.

For a direct v0.3.2-to-v0.3.4 upgrade, use the exact Steward v0.3.4 tag to run
the 1.2.0-to-1.2.1 and 1.2.1-to-1.2.2 provider-profile transitions in order.
No intermediate Helm upgrade to Steward v0.3.3 is required.

#### Required action

Before every execution-enabled v0.3.4 upgrade, create the fixed
`steward-workflows` namespace and add it to `runtimeNamespaces`. The chart now
rejects execution-enabled values that omit this namespace; values copied from
the earlier `[steward-tasks]` example must be updated before `helm upgrade`.

## [0.3.3] - 2026-09-28

### Changed

- Documented the supported stock OpenShell v0.0.98 sidecar topology, Kubernetes
  1.35 init-container sideload setting, sandbox SPIFFE identity, lazy provider
  token grants, and supervisor decision diagnostics.

### Fixed

- Made the platform preflight inspect the rendered tools profile, require its
  MCP-GW endpoint to match the connections bridge, reject unsupported
  unrestricted, IPv4 loopback, IPv4 link-local, IPv6 unspecified, and IPv6
  loopback CIDRs, and require the released connections-bridge binary.
- Published provider-profile bundle 1.2.1, which permits the POST transport
  required by read-only MCP operations and renders one deployment-configured
  Mint audience for both MCP and inference grants, with an exact 1.2.0 upgrade
  transition.
- Normalized an exact OpenAI Responses operation URL to the Codex base URL so
  Codex does not append a second `/responses` path segment.
- Returned bounded, versioned connection-operation errors that distinguish an
  OpenShell proxy policy denial from an MCP-GW runtime-authorization denial.

### Known issues

- The v0.3.3 apiserver rejects an empty
  `config.apiserver.anthropicInferenceEndpoint` during startup even when no
  `claude-code-v1` execution binding is configured. Before upgrading to
  v0.3.3, set this field to the deployment's Anthropic-compatible LiteLLM base
  URL. Steward v0.3.4 removes this temporary workaround for Codex-only
  installations.

### Upgrade from 0.3.2

This patch has no breaking chart or runtime change, changes no chart default,
and adds no database migration. It does tighten the installation preflight:

- `execution.endpoints.mintAudience` is now required and must equal
  `config.mint.audience` (`steward-mcp` by default);
- provider CIDRs must pass the stricter checks listed above; and
- provider-profile bundle 1.2.1 is required.

For a v0.3.3-only rollout, the historical bundle 1.2.1 procedure uses the
Steward v0.3.3 tag. A direct v0.3.2-to-v0.3.4 upgrade must instead use the exact
v0.3.4 tag for both ordered bundle transitions and does not require an
intermediate v0.3.3 Helm upgrade. Re-apply both profiles under their existing
IDs, rerun platform preflight, and use the regenerated values and
execution-binding digests for the Helm upgrade. See the
[bundle upgrade procedure](config/provider-profile-bundle/v1.2.1/README.md).

## [0.3.2] - 2026-09-28

### Changed

- Raised the supported Kubernetes floor to 1.32 and aligned the chart and
  installation guidance with the tested Kind 1.32.1 lane.
- Presented the public web application consistently as Steward and reused the
  shipped Steward icon for the application brand and favicon.

### Fixed

- Required the complete seven-route public apiserver path set whenever the
  chart renders Gateway API HTTPRoutes, preventing an incomplete edge from
  silently routing API requests to the web frontend.
- Rejected the empty-suffix organization identifier `org_` consistently in
  the runtime parser, Helm schema, and platform preflight.

## [0.3.1] - 2026-09-27

### Changed

- Completed the Steward browser workspace across request review, Envelope
  management, governed runs and logs, connections, settings, and guided
  onboarding with a consistent responsive component system.

### Fixed

- Enforced the API's `org_` organization identifier contract in the Helm schema,
  platform preflight, and shipped examples.
- Generated only the seven supported public HTTP routes instead of the obsolete
  catch-all `/api` route.
- Corrected core-mode installation guidance: external Task submission is
  rejected before Task or runtime creation until orchestration is enabled, and
  browser authentication is not a core-binary prerequisite.

## [0.3.0] - 2026-09-27

### Added

- Added the Steward browser/API redesign: one typed administrator request
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

[Unreleased]: https://github.com/apelogic-ai/steward/compare/v0.3.11...HEAD
[0.3.11]: https://github.com/apelogic-ai/steward/compare/v0.3.10...v0.3.11
[0.3.10]: https://github.com/apelogic-ai/steward/compare/v0.3.9...v0.3.10
[0.3.9]: https://github.com/apelogic-ai/steward/compare/v0.3.8...v0.3.9
[0.3.8]: https://github.com/apelogic-ai/steward/compare/v0.3.7...v0.3.8
[0.3.7]: https://github.com/apelogic-ai/steward/compare/v0.3.6...v0.3.7
[0.3.6]: https://github.com/apelogic-ai/steward/compare/v0.3.5...v0.3.6
[0.3.5]: https://github.com/apelogic-ai/steward/compare/v0.3.4...v0.3.5
[0.3.4]: https://github.com/apelogic-ai/steward/compare/v0.3.3...v0.3.4
[0.3.3]: https://github.com/apelogic-ai/steward/compare/v0.3.2...v0.3.3
[0.3.2]: https://github.com/apelogic-ai/steward/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/apelogic-ai/steward/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/apelogic-ai/steward/compare/v0.2.6...v0.3.0
[0.2.6]: https://github.com/apelogic-ai/steward/compare/v0.2.5...v0.2.6
[0.2.5]: https://github.com/apelogic-ai/steward/compare/v0.2.4...v0.2.5
[0.2.4]: https://github.com/apelogic-ai/steward/compare/v0.2.3...v0.2.4
[0.2.3]: https://github.com/apelogic-ai/steward/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/apelogic-ai/steward/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/apelogic-ai/steward/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/apelogic-ai/steward/compare/v0.1.23...v0.2.0
