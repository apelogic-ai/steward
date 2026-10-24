# Changelog

All notable changes to Steward are documented in this file.

The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed

- Chart publication now captures Helm output from both output streams, and the pull-request release
  candidate uses the same package, push, and digest-validation script as tag-time publication.

## [0.3.23] - 2026-10-23

Steward v0.3.23 contains the v0.3.22 release content plus a release-workflow fix that pins Helm for
chart publication and release artifact assembly. The v0.3.22 tag was not fully published: its
governed dependency charts and `steward-bundle` were never pushed, and it has no GitHub release.

This release adds a hardened `linux/amd64` Claude Code reference runtime with immutable release
evidence, separately installable governed dependency charts for SPIRE, agent-sandbox, OpenShell,
LiteLLM, and Actions Runner Controller, and a bundle chart that installs them with Steward as one
release. Security maintenance updates Tokio to resolve RUSTSEC-2026-0386.

### Added

- Added the optional `steward-bundle` OCI chart for installations that need one
  Helm release for Steward, SPIRE, agent-sandbox, OpenShell, LiteLLM, and
  optionally Actions Runner Controller. Every child chart is independently
  switchable, creates no Namespace, uses digest-pinned images, and shares one
  existing Secret without accepting inline secret material. The bundle keeps
  the standalone Steward render unchanged, packages CRDs before dependent
  objects, disables SPIRE's test-only identities, and requires the shared TLS
  certificate to be supplied in that Secret rather than generated into it. It
  is rendered and installed in a clean cluster by the release gates before
  publication and after pulling the published artifact.
- Added separately installable, release-pinned charts for SPIRE, agent-sandbox,
  OpenShell, LiteLLM, and Actions Runner Controller. They use upstream chart
  dependencies where available, create no Namespace, accept existing Secret
  references instead of inline secret material, pin every rendered image by
  digest, and document cluster-wide permissions and Pod Security needs. The
  release publishes each chart as its own OCI artifact alongside Steward and
  lists their immutable coordinates in the release handoff. SPIRE CRDs are
  installed before dependent resources, agent-sandbox wraps its upstream Helm
  chart, and the LiteLLM, SPIRE, and Actions Runner Controller wrappers refuse
  free-form environment inputs. LiteLLM also refuses database URLs containing
  passwords; its default workload security contexts satisfy a
  restricted Pod Security profile. OpenShell's optional GRPCRoute requires an
  existing Gateway and hostname; streaming timeouts remain configured on the
  Gateway or backend because GRPCRoute has no timeout field. Existing Steward
  chart SBOM and vulnerability asset names remain available.
- Releases now include a `linux/amd64` Claude Code reference runtime for 2.1.222, its immutable digest,
  SBOM, vulnerability report, and provenance. The non-root image includes the same pinned read
  tools as the Codex runtime and uses the existing `claude-code-v1` execution binding. Its launcher
  accepts only the deployment-provided Anthropic-compatible LiteLLM URL and OpenShell placeholder,
  disables updates and nonessential traffic, removes alternate Claude authentication paths, and
  exits 125 before startup when those settings are missing or changed. Operators must mirror the
  new `referenceRuntimes.claudeCode` artifact and use its digest in the binding before activation.

### Security

- Kept ES256 test-key generation dependencies out of shipped binaries. A production dependency
  check now prevents `p256` and affected `crypto-bigint` 0.5 releases from entering those binaries;
  `RUSTSEC-2026-0387` remains narrowly ignored only while test tooling requires it.
- Update Tokio to 1.53.3 for RUSTSEC-2026-0386 (`AsyncFd` I/O-safety unsoundness).

### Fixed

- Chart publication and release artifact assembly now use the repository's pinned Helm version,
  preventing release output from changing when a newer Helm major version becomes available.

### Upgrade and rollback

- No database migration is required when upgrading from v0.3.21. Deploy the chart and component
  images as one release unit, mirror the digest-pinned Claude Code reference runtime, and update
  `claude-code-v1` execution bindings to that digest before activating it.
- The governed dependency charts are separate, opt-in release artifacts. Before adopting one,
  provide its target namespaces, existing Secret references, required cluster-wide permissions,
  and any Gateway configuration documented by that chart. The existing Steward chart keeps its
  prior default rendering.
- Before rolling back to v0.3.21, stop new Claude Code Tasks and let active Tasks finish, then
  restore execution bindings to a reference runtime supported by that release. Remove dependency
  charts only when the installation adopted them solely for v0.3.23, and restore its previous
  dependency ownership. No database rollback is required.

## [0.3.22] - 2026-10-23

The v0.3.22 tag was only partly published. Component images, both reference runtimes, and the
`steward` chart were published, but the governed dependency charts and `steward-bundle` were never
pushed, and no GitHub release was created. Use v0.3.23 for the complete release.

## [0.3.21] - 2026-10-22

Steward v0.3.21 adds owner-scoped personal agents with durable conversations, resumable updates,
and bounded execution through configured coding agents and Envelopes. Installations can also
accept carefully scoped user access tokens from configured issuers and advertise reviewed
Kubernetes command and remote-shell grants.

The Helm chart can use independently promoted component repositories and tags, render public
trust material, and configure optional HTTPRoute timeouts. Security maintenance pins the
latest-upstream LiteLLM image to an immutable digest and refreshes the bundled web trust roots.

### Security

- Updated the web runtime to Next.js 16.4.1 while retaining Sharp 0.35.5, resolving two critical
  unauthenticated code-execution vulnerabilities in the deployed web image.
- Pinned the latest-upstream LiteLLM conformance image to a reviewed immutable digest. CI now
  refuses mutable LiteLLM images in scripts, workflows, Compose files, and chart values.
- Updated certificate verification and the bundled web trust roots to reject certificates chained
  through distrusted emSign intermediates.

### Added

- Installations can advertise governed `kubectl` read, write, and destructive classes plus remote
  shell in the deployment capability catalogue. Envelope requests and templates select those exact
  grants through the existing API-backed editor, Task admission allows any exact subset, and the
  new operator guide explains how a trusted provisioner binds a machine to the Run runtime ID.
  These command routes require MCP-GW 0.7.0 or later.
- The Helm chart can now set independent request and backend-request timeouts
  on its API and web HTTPRoutes. Timeout fields remain absent by default, so
  existing route behavior is unchanged until an operator opts in.
- The Helm chart can now create the public task-token JWKS and gateway backend
  CA ConfigMaps from values. It rejects malformed public material and private
  keys, while unchanged values continue to reference separately managed
  ConfigMaps. Core deployments can also reuse one existing Secret for the
  database URL and both TLS mounts.
- The Helm chart now accepts a repository for each component and can use the
  chart application version when a component tag is empty. Digest pinning
  remains the default; hosting platforms that verify promoted tags can opt in
  to repository-and-tag image references.
- Optional headless user access accepts RS256 or ES256 JWT access tokens from an exact configured
  OIDC issuer on `/app/api/v1/*` when the HyperShell audience, frontend client IDs, and maximum
  token age are configured. The portal audience and token `typ` are separate opt-ins. Tokens reuse
  canonical users and local RBAC, ignore provider groups, never grant administrator-route access,
  and persist the issuer and frontend client ID without recording the token. The stable user-ID
  and email claims are configurable; operators may either require `email_verified` or explicitly
  trust the issuer's email, which also permits exact same-organization email linking to an existing
  canonical user that has no other subject from that issuer. `email_verified` is reserved from
  identity-claim mapping, and an explicit false value is always refused. Google browser sign-in
  never email-links an active user, and Google access tokens remain browser-session only because
  Google issues opaque user access tokens. One shared, default-on setting controls first-sign-in
  creation for both browser and access-token users; disabling it still permits pending-invitation
  activation and configured trusted-email linking but never creates a new canonical user. Every
  bearer-authenticated request emits one sanitized structured-log audit record before handler
  dispatch with its route template and authorization outcome; verified tokens refused before a
  canonical user is resolved record a null canonical-user field. Deployment log shipping provides
  retention. Tasks retain the same verified issuer and frontend client ID. Authorization values
  and tokens are never recorded. Email trust is safe only when users cannot change their own email
  at the identity provider.
- Signed-in users can create owner-scoped personal agents from a configured coding agent and an
  active Envelope, post idempotent messages, and read ordered turns linked to their normal Runs.
  The dispatcher merges messages that arrive during a turn into one next turn, resumes the same
  Run after restart, and always requests one selected Envelope model without tools. Envelopes with
  multiple models require an explicit choice. Deployment settings bound concurrent turns,
  Task-start attempts, unexpected restarts, retained conversation turns, leases, status checks,
  total monitoring time, and every content size. A resumable event stream sends durable status and
  final-reply updates, with heartbeat and no-buffering headers for the browser edge path.

### Upgrade and rollback

- Apply additive migrations 0074 and 0075 before starting v0.3.21. Migration 0074 adds the
  owner-scoped personal-agent, turn, message, and event tables. Migration 0075 bounds retries for
  interrupted turns. Neither migration rewrites historical rows. Deploy the chart and all
  component images as one release unit.
- Existing chart values retain their prior rendering unless an operator enables a new option. To
  use user access tokens, configure the exact issuer, audience, frontend clients, token age, and
  identity claims. Governed Kubernetes commands and remote shell require MCP-GW v0.7.0 or later
  plus a trusted provisioner that binds the machine to the Run runtime ID.
- Before rolling back to v0.3.20, stop new personal-agent messages and let active turns finish.
  Remove v0.3.21-only chart values before installing the older chart, and restore externally
  managed public ConfigMaps if the v0.3.21 chart created them. Migrations 0074 and 0075 may remain;
  v0.3.20 ignores those tables. User access-token sign-in, personal agents, and the new command
  grants are unavailable after rollback.

## [0.3.20] - 2026-10-18

Steward v0.3.20 adds governed external Run API callers and signed Jira webhook Triggers, including
an isolated trigger receiver with bounded delivery, replay, cooldown, and cleanup behavior. Run
views can show caller-supplied display context without using it to decide whether a request is
accepted.

Failed Tasks can expose validated partial output and requested bounded diagnostics, and API Runs
now carry an explicit origin. The release also strengthens receiver startup, delivery recovery,
chart rendering, caller refusal coverage, and coding-agent execution evidence.

### Added

- Added a Jira webhook Trigger operator guide covering exact-path exposure, signing Secrets,
  read-only Envelope checks, filters, run limits, cooldowns, response meanings, optional issue
  links, and deployment acceptance requirements.
- Release publication now includes the Jira trigger receiver image, its immutable digest, scan,
  provenance, release handoff entry, and registry-mirror values alongside the core components.
- External Run API callers may attach a bounded display label and optional HTTPS link. My runs,
  All runs, and run details show that context with the service name from the verified Task token;
  the Jira receiver supplies its Trigger id, issue key, and configured-site issue link.
- Added prepared `@callers` journeys for Jenkins and Temporal Run API callers, Jira receiver
  delivery and replay, unbound-caller refusal, missing or invalid Jira signatures, and Trigger
  cooldown. The real receiver process test covers write-capable Envelope refusal without adding a
  deliberately write-capable receiver to local-main. Direct receiver paths now follow the deployed
  response contract; the live Jira path still requires an explicit owner opt-in.
- Added validation, routing, and the ability to start and clean up governed Tasks for signed,
  bounded Jira webhook Triggers. The Helm chart can now deploy that receiver with isolated
  identity, exact Secret and CA projections, bounded runtime settings, and deny-by-default
  network access; operators must expose only each exact `/hooks/jira/<id>` path through their edge.
- Published the generated Run API OpenAPI contract and added Jenkins and Temporal Go examples for
  token exchange, idempotent submission, empty input archives, non-blocking polling, failed-run
  diagnostics, output download, and cleanup. Jenkins support requires the next
  `github-oidc-exchange` release containing generic OIDC exchange.

### Changed

- Tasks submitted through the API without browser or GitHub source evidence now report their Run
  origin as `api`, while existing browser, GitHub Actions, and Connections origins are unchanged.
- Authenticated Task callers can now download validated partial `out/` files after an agent
  process fails. Failed Tasks also include bounded stdout and stderr when they requested full
  diagnostics; missing or disabled diagnostics remain private, and browser output behavior is
  unchanged. A terminal failure with nothing to download returns `204 No Content`.

### Fixed

- Exact Jira webhook replays now return `200 No run needed` even when a retry reaches a different
  receiver process or the original process restarted. Only a new PostgreSQL Task reservation can
  return `202 Accepted`; a duplicate never uploads inputs, starts, or finalizes the existing Task,
  and it does not consume the receiving process's cooldown or concurrency limit.
- Caller journeys now use a protected short-lived administrator token for Run checks, so Jenkins,
  Temporal, and Jira acceptance can run without a person's browser session or putting the bearer
  in Playwright traces.
- Steward binaries now select their shared TLS crypto provider before constructing clients or
  servers, preventing the Jira receiver from panicking during startup.
- The Helm chart now renders every Jira receiver numeric environment setting as decimal digits,
  preventing the receiver from rejecting large default or ceiling values written in scientific
  notation.
- The Codex reference runtime now removes inherited C build tools and kernel headers after
  installing its prebuilt tools. Its release scan still fails on critical findings and verifies
  that the header package is absent.
- Jira webhook delivery now recovers its concurrency and cooldown state if Task startup stops
  unexpectedly, treats semantically identical JSON retries as one delivery, reuses bounded
  Kubernetes access credentials within one Task lifecycle, requests a fresh workload assertion for
  each token exchange, and retries temporary Task API timeouts. The optional delivery identifier is
  no longer required.
- Bound Agent Tasks now succeed when the agent exits successfully and writes every declared
  output but leaves its conversational final message empty. Missing declared output still fails
  with the output-missing reason.
- Task authentication now refuses credentials that combine a GitHub Actions identity with a
  pure-service caller, while delegated GitHub callers continue to work.
- Coding-agent launches now record that execution started before any terminal result, including
  when execution logs are disabled. A launcher refusal is reported as `agent-start`, while an
  approved tool refusal that prevents required output is reported as `tool-failed` instead of
  `output-missing`; disabled execution logs remain unavailable.

### Upgrade and rollback

- Apply additive migrations 0072 and 0073 before starting v0.3.20. Migration 0072 accepts the new
  `api` Task origin without rewriting historical rows. Migration 0073 adds nullable immutable
  caller-reference context without rewriting historical rows. The Jira Trigger receiver uses
  `config.apiserver.jiraBaseUrl`, when set, as the validated HTTPS site and optional context path
  for issue links; without it, runs retain a label without a link. Issue display metadata never
  determines whether a delivery is accepted. Deploy the chart, API, web, and receiver images
  together.
- Before rolling back to v0.3.19, stop Task submission and prove that no stored Task has the `api`
  origin; v0.3.19 cannot read that new immutable value. If any such row exists, restore the
  pre-upgrade database or keep v0.3.20 API readers. The nullable caller-reference column and its
  values may remain because older API and web versions ignore them. A full chart rollback removes
  the receiver and its configuration.

## [0.3.19] - 2026-10-15

Steward v0.3.19 gives Run clients safe explanations for bounded-workspace preparation failures and
keeps expected runtime-deletion progress out of error logs without hiding stalled cleanup.

### Fixed

- Run APIs now return the bounded workspace-preparation explanation stored for a failed Run, so
  clients can show why the coding agent did not start without exposing internal diagnostics.
- Runtime cleanup now retries quietly while sandbox deletion is making progress, while a deletion
  that remains incomplete past the cleanup deadline still produces a controller error.

### Upgrade and rollback

- This release adds no database migration and requires no configuration change. Deploy the v0.3.19
  chart and all component images as one release unit.
- Rolling back to v0.3.18 hides the two bounded workspace-preparation explanations from Run API
  responses and restores error logging during expected sandbox-deletion progress. The cleanup
  deadline and stalled-deletion error remain unchanged.

## [0.3.18] - 2026-10-15

Steward v0.3.18 adds catalog-defined provider tools and provider-specific Connections, including
Jira sign-in and read-only Jira tools. It also makes governed repository Tasks find their
checkouts reliably, strengthens per-run inference-limit accounting, and improves browser
explanations for failed or refused Runs.

### Fixed

- Provider sign-in now opens in a new tab and refreshes connection status when users return, while
  retaining a same-tab fallback when the browser blocks the new tab.
- Provider display names in the deployment capability catalog no longer prevent the token service
  from starting; unsupported provider fields are still refused.
- Provider connection cards now use the catalog display name and available actions consistently.
  Active and renewal credential expiry values show a local date and time, invalid values are
  reported as unavailable, and a connected account without a reported renewal deadline says that
  it renews automatically.
- Jira sign-in now accepts the gateway's one-time state alongside its consent URL while still
  refusing incomplete or unexpected responses.
- Coding agents now receive the exact path of every repository workspace in their prompt, so a
  plain request about a repository can find its files while staged caller input and declared
  outputs keep their existing relative paths. A run that creates no regular file beneath `out/`
  now fails with a plain explanation instead of succeeding with no downloadable output. Task
  pages show known repository checkout paths and identify paths resolved when a Run starts; Run
  pages show every resolved checkout path and the missing-output explanation. Both surfaces
  identify read-only checkouts.
- Claude Code Tasks without connected tools now start normally, and Runs that cannot start
  because of invalid coding-agent settings show a plain explanation.
- Per-run inference limits now refuse zero before a Run starts, cap each runtime's gateway key,
  account for spend already consumed by the same Envelope instance, and fail the Run with a plain
  reason when its accepted model or spending settings are later rejected. The API and Run page
  show the spending-limit reason and exhausted final spend even when the first model call exceeds
  the limit or its charge settles after the agent exits; spending in the last few seconds can
  exceed the configured amount. Runs with a per-run limit can wait up to 20 seconds for final spend
  to settle before their runtime key is removed.
- Task cleanup now records one final inference-spend observation before deleting the run key, so
  short runs no longer finish with stale zero spend or disappear from monthly Envelope usage.
- Approving a custom or template Envelope request that exactly matches authority the user already
  holds now reuses the active Envelope and terminalizes the duplicate request.
- The administrator request queue now reports whether external decision filing is enabled, so
  browser clients can hide a filing action that cannot succeed.
- Browser pages now use plain-language run stages and refusal reasons, show Envelope display names,
  runtime limits, actor names, and save confirmation, and start new templates without implicit
  roles, models, platforms, or inline browser-authored packages.
- Workspace preparation failures now fail Runs with a plain explanation and a bounded controller
  diagnostic instead of `execution_outcome_unknown`.
- Rejected Envelope-template saves now return a stable error code, a plain explanation, and the
  affected form fields instead of an empty 422 response.
- A Task's requested runtime-minutes limit now caps that run independently of the Envelope's
  monthly runtime allowance, and Run details show both limits and current monthly usage. The
  controller checks this cap during reconciliation, so enforcement can occur up to about one
  minute after the cap is reached.
- Custom Envelope workspace controls now keep the complete JSON preview identical to the request.
- The Envelopes Pending tab now counts and shows only requests that are still awaiting a decision.
- Scratch workspace capacity mismatches now fail Runs with an actionable storage explanation
  instead of `execution_outcome_unknown`.
- Cancelling a running Task now records a cancelled Run without replacing the cancellation with
  an `execution_outcome_unknown` failure when the sandbox cannot prove the stopped process's exit.
- Envelope requests now offer an optional monthly runtime limit when the selected template has no
  runtime limit.
- Workspace branch and tag fields now preserve their entered value, reject unsupported selectors
  with inline guidance, and name a missing ref and repository when a Run is refused. GitHub's
  missing-ref response is recognized, and other Run refusals no longer receive unrelated
  Envelope-limit advice.

### Changed

- Connections can now route direct status calls and non-GitHub sign-in starts such as Jira to a
  provider-specific MCP-GW wrapper, while preserving the existing default for providers without a
  mapping. GitHub start and disconnect keep their governed bridge flow.
- Connections can now start sign-in for enabled catalog providers such as Jira. Steward returns
  the provider's one-time consent URL and grants the gateway only the selected provider's start
  action for that request; GitHub keeps its existing governed connection flow.
- Runtime-free connection-status tokens can now carry one exact deployment-catalog provider's
  status tuple, allowing provider-neutral status reads without granting provider mutations or
  ordinary tools. Requests from older API replicas retain the previous GitHub-only tuple during
  rolling upgrades.
- Deployment capability catalogs now support tool choices from any configured provider, including
  Jira read tools, in Templates, Envelopes, Tasks, and Runs. Each Run receives only the tools its
  Task approved. The Connections API now lists each available catalog provider with its configured
  display name and supported actions, and reads each status independently through the gateway's
  stable provider route. Disabled providers are not returned as connection rows.
- Added the first real-installation browser journey for creating the initial Envelope templates,
  together with the ten-journey feature specification and a tag-selectable Playwright runner.
- Web spend and budget amounts now show two decimal places and their currency. Positive amounts
  below 0.01 show as `< 0.01` with the currency instead of rounding to zero.
- New Tasks, Run now, and Get started choose their initial coding agent and model from deployment
  configuration and the selected Envelope instead of list position. Saved Task versions remember
  their selected Envelope for later runs.
- Running Tasks show stdout and stderr together while in progress, and the displayed run duration
  now advances until the run finishes.

### Security

- Updated `rustls` to 0.23.46 to remediate RUSTSEC-2026-0349.

### Upgrade and rollback

- Apply additive migration 0071 before starting v0.3.18. It adds an optional preferred-Envelope
  digest to immutable browser Task versions; existing rows remain unchanged with no preference.
  Retain the added column and its historical rows during rollback.
- Jira Connections require MCP-GW v0.6.0 with its read-only Jira REST preset. Configure the exact
  Jira wrapper under `connectionsBridge.providerOrigins` before enabling Jira in the capability
  catalog. During a rolling upgrade, replace every old Mint pod before adding catalog
  `displayName` fields. Before rolling back to v0.3.17, remove every catalog `displayName`, disable
  Jira and remove its tool and catalog entries, and remove provider-specific origins from the older
  chart's values.
- Task evidence gains the optional `taskRuntimeMinutesLimit` field. After a Task with an explicit
  per-run cap stores this evidence, v0.3.17 and earlier binaries cannot read that row. Before such
  a rollback, stop Task submission and prove that no stored Task evidence carries the field.

## [0.3.17] - 2026-10-13

Steward v0.3.17 makes administrator approval the authority boundary for Envelope requests outside
a template, restores reliable browser mutations and first-template creation, returns stable and
human-readable Run refusal errors, and makes bounded Git workspace preparation fail promptly and
accurately.

### Changed

- Git workspaces with bounded history now accept the shallow-boundary packets emitted by Git
  servers, so files-only and explicit-depth checkouts are no longer refused.
- Run-request refusals and temporary unavailability now return stable error codes separately from
  their plain-language failure reasons.
- A failed workspace-source download now immediately fails the reserved run with its source cause,
  prevents runtime provisioning, and records the source-operation failure. Browser submission and
  the Task-input upload handler return the same bounded 503 explanation.
- New Envelope templates now start with and validate a USD budget currency, so administrators can
  create the first template through the browser UI.
- Browser run cancellation and managed inference-key removal now preserve the required JSON
  mutation proof when their request payload is empty.
- User Envelope requests within a template still provision automatically. Requests above a
  template and complete template-free requests now remain pending for an administrator without a
  second platform safety ceiling. For requests that need review, model, tool, and workspace-type
  availability is checked at both request creation and approval, and rejections return a plain
  sentence plus affected fields. Within-template auto-provisioning remains governed by the
  published template rather than the descriptive capability catalog.
- Administrators can approve or reject an Envelope request directly without first filing an
  external decision reference.

### Removed

- Removed `platform_safety_ceiling_exceeded` and `platform_safety_ceiling_unconfigured` request
  failures. Administrator approval is the control for authority outside a template.

### Upgrade and rollback

- `config.apiserver.customEnvelopeSafetyCeiling` remains schema-compatible but is ignored in this
  release. A configured value produces one apiserver startup warning. Remove the key before the
  next release, when the chart schema will reject it. Existing pending requests are retained and
  become approvable subject to current deployment capability availability.
- Above-template and template-free Envelope requests now require a non-empty justification. A
  missing justification is rejected with HTTP 422 and code `justification_required`.
- Refused and temporarily unavailable Run requests now return a stable code in `error` and the
  plain sentence in `failureReason`; clients must not parse internal error formatting.
- Git workspace checkout requires the selected Git host to be reachable from the apiserver in
  addition to any provider API endpoint.
- This release adds no database migration. A database used by v0.3.17 remains readable by v0.3.16,
  but rollback restores v0.3.16 safety-ceiling behavior for pending Envelope requests.

## [0.3.16] - 2026-10-12

This release requires a coordinated upgrade of the agent catalog; follow
[Upgrade and rollback](#upgrade-and-rollback) before resuming Task submission.
It makes coding-agent and model compatibility deployment-configured, adds live
execution-log persistence, accepts stable workspace repository and ref names,
and simplifies Envelope request limits and administration. Additive migrations
0069 and 0070 preserve request review context and retire the separate
automatic-provision threshold without rewriting provisioned Envelopes.

### Added

- The supported Codex reference runtime now includes pinned `rg`, `jq`, `fd`,
  and `tree` commands for repository inspection and structured-output work.
- Execution binding catalogs now declare supported model providers. Signed-in capabilities expose
  that compatibility, and Task planning rejects an incompatible agent/model selection before
  reservation without hard-coded provider families.
- Browser Run now, Get started, and the Task editor now offer an exact model picker derived from
  the selected Envelope and the selected coding agent's configured providers. Existing Tasks
  preserve their explicit runtime model and requirements until the author changes the selection;
  drafts remain editable without an active Envelope. Newly configured provider/model names appear
  without a Steward code change.
- Signed-in users can read the deployment's enabled workspace types and
  numeric workspace limits from `/app/api/v1/capabilities`. The response does
  not expose administrator configuration or other users' data.

### Changed

- Task admission now accepts requested values at or below every effective
  Envelope limit, including scratch sizes smaller than the deployment volume
  and `history: full` within the allowed Git-history depth. A scratch request
  is rejected when the deployment's fixed scratch volume is larger than the
  Envelope permits. Requested and effective values remain in immutable Task
  evidence.
- Envelope template and custom-request forms now offer deployment-configured
  workspace types as checkboxes with an optional total-size limit; the other
  workspace limits default to the deployment's settings. Template requests and
  Envelope details show those effective deployment limits without rendering
  missing values. Existing narrower file, history, and submodule limits remain
  unchanged when those forms are edited. Administrator approval comparisons
  always identify omitted limits as deployment limits, and stored size values
  that need correction can no longer be cleared implicitly by an unrelated
  edit.
- The published Task API schema and generated web client now include the
  `owner/repository`, plain branch or tag, and `git:ref:<name>` workspace
  selectors already accepted by the server.
- OpenShell Agent Tasks with `diagnostics.executionLog: full` now run in a
  restart-safe, attempt-scoped process group. Steward persists bounded stdout
  and stderr snapshots while the Task runs, refreshes Run detail before the
  terminal state, and retains final output for successful and failed attempts.
  The bounded JSON execution-log response adds the required `partial` field:
  `complete` now means the Task is terminal, while `partial` identifies the
  last live snapshot when no final transcript was saved, such as after some
  cancellations or unknown outcomes. Unchanged snapshots do not rewrite the
  stored log.
- Compatible Agent images that use full execution logging must expose Linux
  procfs at `/proc` and provide the documented shell utilities, including
  `setsid`, `awk`, `head`, and `tail`. The detached process output is available
  through authenticated Run-log routes; `openshellTaskLogMode=full` mirrors
  only synchronous task processes into controller logs.
- TaskDefinition v2 Git workspaces may identify an already-admitted repository
  by its case-insensitive `owner/repository` name or by an HTTPS link on the
  configured source host. Steward resolves only operator-bound stable IDs,
  records the provider's canonical identity and exact commit, and reuses that
  snapshot on idempotent retries. Browser Run now and the Task editor author
  these repository names and plain branch or tag refs directly.
- `ref: "trigger"` now resolves correctly for an admitted repository other
  than the Task's own source repository, resolving the selected repository's
  default branch rather than reusing the Task source ref.
- Envelope Templates now have one boundary: requests within the published
  limits provision automatically, while requests above those limits enter the
  administrator approval queue when they remain within the deployment safety
  ceiling and capability catalog. Above-template requests require a
  justification and preserve the exact exceeded values for requester and
  administrator review. To preserve existing review policy, migration 0070
  appends a narrower successor revision for each current template whose retired
  `autoProvisionThreshold` was stricter than its limits. Templates without a
  threshold are unchanged, pending requests remain pending, and already
  provisioned Envelopes are unaffected.
- Historical template revisions continue to expose `autoProvisionThreshold` on
  reads. New template revisions omit it, and browser or operator writes that set
  it are rejected with an actionable error. To retain the old behavior of
  reviewing anything above 50 USD, set the template's monthly limit to 50 USD.
- Admission failures now name limits and units in user-readable language,
  including readable binary workspace sizes, without exposing internal field
  paths or debug formatting.
- Envelope screens now let users edit the per-run budget limit, summarize every
  requested value above a template, show effective workspace access, and use
  consistent plain-language labels and units across request, detail, and
  administrator views.

### Upgrade and rollback

- **Required coordinated agent-catalog upgrade:** move every active catalog from
  `steward.execution-bindings/v1` to `steward.execution-bindings/v2`, with
  `models.providers` declared for every agent. Upgrade the catalog, chart,
  apiserver, and controller in one coordinated step with Task submission and
  Workflow publication paused. Deployment-preflight input must add
  `execution.binding.modelProviders`, covering every provider in
  `capabilityCatalog.models`. Resume only after every old apiserver and
  controller pod is gone. The field changes the binding digest; persisted v1
  bindings on already-admitted Tasks remain readable. Once v0.3.16 creates any
  Task, do not downgrade: previous binaries cannot read its v2 binding and
  additive evidence fields. Before the first new Task, restore the previous
  binaries and exact v1 catalog together while submission remains paused. See
  the [execution-binding upgrade procedure](docs/installation/upgrade-execution-bindings.md).
- Migration 0070 is append-only and records every generated template revision
  with actor `system:migration`; retain it on rollback. List the affected
  template IDs and successor revisions with
  `SELECT template_id, revision FROM envelope_template_revisions WHERE authored_by = 'system:migration' ORDER BY template_id, revision`. During a mixed-version
  rollout, migration 0070 accepts the equal-to-limits threshold emitted by the
  previous default-template writer but rejects every distinct threshold. If a
  one-boundary binary creates a default-template revision, disable
  `config.apiserver.defaultLlmTemplate.enabled` before rolling back to v0.3.15;
  the previous binary will not start with that setting enabled against the
  threshold-free revision. In addition, v0.3.16 Task evidence records requested
  Task requirements, runtime minutes, and the effective full-history depth
  under the existing evidence schema. After v0.3.16 creates a Task, do not
  downgrade to v0.3.15: that binary cannot read the added evidence fields. Do
  not delete generated revisions or rewrite pending requests and provisioned
  Envelope history.

## [0.3.15] - 2026-10-09

This release adds governed Git and scratch workspaces to TaskDefinition v2 and
activates per-user managed inference credentials through Mint. It also promotes
the MCP-GW agentgateway path from an indirect compatibility assumption to a
real governed-connections E2E against the pinned gateway. Migration 0068 is
additive and intentionally retained on rollback.

### Added

- TaskDefinition v2 supports governed `git` and `scratch` workspace entries.
  Steward resolves exact Git commits before runtime creation, applies deployment,
  approved User Envelope and per-entry limits, materializes credential-free working
  data beneath `/sandbox/workspace`, and records immutable workspace evidence.
  Once workspace-bearing Task evidence is stored, rollback to a pre-workspace
  Steward release requires restoring the database from a compatible backup.
- Managed-inference custody foundation: additive migration 0068 stores one
  encrypted credential per canonical user plus append-only audit events. The
  migration is intentionally retained on rollback. Authenticated users can
  create, replace, inspect metadata for, and delete their own credential via
  `/app/api/v1/connections/inference`; plaintext credential material is never
  returned after submission.
- Steward 0.3.15 activates `inference.mode: managed`. The chart projects the
  dedicated `secrets.managedInferenceDatabase` reader credential into Mint and
  the `secrets.managedInferenceEncryptionKey` file into only the apiserver and
  Mint. Mint uses a least-privilege reader role for
  `managed_inference_credentials`; stock mode remains the default and projects
  none of this managed material.
- Managed model Tasks fail before reservation with `inference_key_missing` or
  the user-actionable `inference_key_unusable`. Switching between stock and
  managed mode requires stopping submissions, draining or revoking existing
  runtimes, changing the chart configuration, and verifying a fresh Task;
  live runtimes are never switched in place.
- The governed-connections E2E now deploys the MCP-GW 0.5.7 agentgateway used
  by the platform, proves session enforcement, and exercises repository search,
  publication, workflow detection and dispatch through that gateway. The
  direct-wrapper case remains covered.

### Upgrade and rollback

- Deploy the v0.3.15 chart and all component images as one release unit. Run
  migration 0068 before enabling managed inference. Managed mode requires the
  dedicated Mint PostgreSQL reader credential, the managed-inference
  encryption-key Secret, and PostgreSQL egress when NetworkPolicy is enabled.
- Before switching an installation from stock to managed inference, stop new
  submissions and drain or revoke existing stock runtimes. The user's upstream
  key remains long-lived; the Mint grant lifetime is only a cache lifetime and
  Steward cannot revoke that key at the upstream inference service.
- Workspace policy is configured by the chart's `workspace` values and bounded
  again by each User Envelope and Task. Keep `workspace.scratchVolumeSize`
  aligned with OpenShell's workspace storage limit so the scratch bound remains
  kernel-enforced.
- Rolling back the binaries and chart leaves migration 0068 in place. After a
  Task with workspace evidence has been stored, rollback to v0.3.14 or earlier
  requires restoring a database backup from before that Task, because those
  releases cannot read the new evidence shape or framed workspace input.

## [0.3.14] - 2026-10-08

This patch makes governed GitHub operations work again on deployments whose
Connections bridge reaches GitHub through the MCP-GW agentgateway origin, adds
the `bridge-gateway-session` failure category, and adds an unwired Mint
resolver for managed inference credentials. It adds no migrations and changes
no Helm values, chart defaults or provider-profile contracts.

**Impact of the fixed defect (issue 330):** on Steward 0.3.13 and earlier,
including 0.3.12, a deployment whose `connectionsBridge.mcpGatewayOrigin` is
the MCP-GW agentgateway (a session-enforcing MCP gateway) could not publish a
task definition to GitHub, detect its workflow, dispatch it, read its run
status, rerun it, or search repositories with an explicit query; every such
operation failed with `bridge-gateway-http` and upstream status 400, and Get
started's publication step returned HTTP 503. Deployments using a direct
GitHub wrapper origin were not affected. Connect, status and disconnect use
MCP-GW's REST routes and kept working. With `githubSource` bound, the 0.3.13
blank-query admitted repository listing runs no bridge operation, so the picker
kept working and the failure surfaced only at publication or detection; on
earlier releases, and on 0.3.13 without bindings, the blank-query listing uses
the bridge and failed too. The fix was verified against the agentgateway image
pinned by MCP-GW 0.5.7. Upgrade affected deployments to 0.3.14, including the
bridge coordinates (see "Upgrade and rollback").

### Fixed

- The Connections bridge now speaks MCP Streamable HTTP sessions for the
  lifecycle gateway contract (`0.4.9`). Each bridge operation sends
  `initialize` (protocol `2025-06-18`), then `notifications/initialized`, sends
  every tool call of that operation with the `Mcp-Session-Id` the server issued
  and the negotiated `MCP-Protocol-Version`, and ends with a `DELETE` of the
  session. Previously the bridge sent bare `tools/call` requests, so on a
  deployment whose `connectionsBridge.mcpGatewayOrigin` is a session-enforcing
  gateway such as the MCP-GW agentgateway, every governed GitHub MCP operation
  (repository search with a query, workflow detection, publication, dispatch,
  run status and rerun) failed with `bridge-gateway-http` and upstream status
  400.
- A direct GitHub wrapper origin, which issues no session, keeps working: the
  bridge then sends no session header and no `DELETE`. The legacy gateway
  contract (`0.3.2`) keeps its bare rerun request with no handshake.
- Any HTTP 404 to a request that carried a session means the server ended it:
  the bridge re-initializes once per operation and resends the refused call,
  and a second 404 fails the operation. A handshake that fails is never retried
  or reused within the operation.
- JSON and Server-Sent Events replies are both accepted. In an SSE reply the
  bridge reads only `data` fields, ignores `id`, `retry`, `event`, comments,
  priming events without data and server notifications, and requires exactly
  one response whose JSON-RPC `id` matches its request.
- One OpenShell provider-readiness window of 12 seconds now covers each
  operation's MCP session, starting at its first request, instead of restarting
  for every request.
- A session the gateway cannot use (its exact `mcp: invalid session ID header`
  rejection of a request that carried one), a session that ends again after one
  re-initialization, an invalid `initialize` result or protocol version, or an
  invalid issued session ID is reported as the new `bridge-gateway-session`
  failure category (`gateway_session_failed` at the Connections API), with a
  fixed bridge diagnostic such as
  `bridge MCP-GW session could not be established (session rejected)`. Every
  other failure, including any 400 to a request without a session and an
  MCP-GW outage during the handshake, keeps its existing category:
  `bridge-gateway-http` with `failure_detail`, and the credential, authority,
  token-grant, transport and response-size categories. The bridge and the
  control plane must come from this release for the new category to be
  recognized; an older control plane reports it as `bridge_failed`.
- Each MCP operation now makes two more requests (`initialize` and
  `notifications/initialized`). Against a session-issuing gateway the final
  `DELETE` is always sent, bounded to two seconds, and its result is ignored;
  the gateway also expires idle sessions. The session `DELETE` is permitted
  only by provider-profile bundle 1.2.2 (published since 0.3.4), whose MCP-GW
  profile includes `DELETE` in its method set. With an earlier bundle installed,
  the OpenShell proxy denies and logs each session `DELETE`; this is harmless,
  because the operation's result is unaffected and the gateway expires the idle
  session, but install bundle 1.2.2 to stop the denials. No migration or Helm
  value change is required.

### Added

- Mint library: `ManagedCredentialGrantResolver`, a `CredentialGrantResolver`
  for managed inference credentials, and its `ManagedInferenceCredentialSource`
  trait. For the `inference` scope it returns the stored credential of the
  runtime's verified canonical owner, and fails closed with
  `CredentialUnavailable` when the runtime admits no models, has no canonical
  authority, has no acting user or acts for a user other than its owner, or the
  source has no credential. Mint's `AuthorityBinding` gains an `llms` field
  copied from the runtime's admitted models. The resolver is **not wired to a credential store,
  the Mint binary or the browser yet, so there is no behavior change** in this
  release; no configuration enables it.

### Upgrade and rollback

- No migrations since v0.3.13; the newest migration remains 0067. No Helm
  value, schema default, or provider-profile contract change is required.
- Deploy the v0.3.14 chart and all component images as one release unit. The
  fix lives in the Connections bridge, so when `connectionsBridge.enabled=true`
  update `connectionsBridge.image`, `connectionsBridge.sourceCommit`,
  `connectionsBridge.signerIdentity`, and `connectionsBridge.attestationBundle`
  to the values under "Stable bridge provenance inputs" in the v0.3.14 release
  notes; a deployment that keeps an earlier bridge keeps the defect. The
  apiserver, controller and web UI must also come from this release to
  recognize and explain `bridge-gateway-session`.
- Keep or install provider-profile bundle 1.2.2 so the session `DELETE` is
  permitted; an earlier bundle only causes logged, harmless proxy denials.
- Rollback to v0.3.13 needs no database restore because v0.3.14 adds no schema
  or durable-state contract; restore the v0.3.13 chart and its full
  component-image set, including the bridge coordinates, together. Rolling
  back restores the defect on agentgateway origins, and operations already
  recorded with `bridge-gateway-session` read as a generic unavailable failure
  on v0.3.13.

## [0.3.13] - 2026-10-08

This patch makes the browser's default repository picker use the deployment's
admitted source catalog, shares that listing across the browser flow, restores
the execution transcript required by `steward-run` 0.8.1, and adds structured
latency reporting for governed connection operations. It adds no migrations and
changes no Helm values or provider-profile contracts.

### Changed

- With `githubSource` enabled and at least one source in `githubSource.bindings`,
  `GET /app/api/v1/github/repositories` with a blank `query` (the browser
  default in Get started and on the Runs page) now lists the **admitted source
  repositories** directly from the apiserver instead of running a governed
  per-user operation in a bridge sandbox. Organization-owned admitted
  repositories therefore appear by default, and a warm listing no longer waits
  for a sandbox. The apiserver resolves each distinct bound source repository
  ID through the source GitHub App: it lists the App's installations, mints a
  token scoped to that one repository with only `metadata: read`, reads
  `GET /repositories/{id}`, and then revokes that token. Every entry passes the
  governed listing's per-field validation and bounds, must match its bound
  owner and repository IDs, and is marked `ready: true`. The `login` field is empty for this listing, because it
  does not consult the user's GitHub connection.
- Consumers of the default listing must not treat its success as proof of a
  GitHub connection: a user without one now sees admitted repositories marked
  ready, and publication or dispatch can still fail on the connection or on
  OAuth App access. The user's own repositories that are not admitted, which
  0.3.12 listed as not ready, no longer appear in the default listing; an
  explicit `query` still finds them. The admitted listing runs no governed
  operation, so it writes no governed-operation record; workflow detection,
  publication, dispatch and run status still do.
- The admitted listing is cached in process for 10 minutes per binding
  configuration. After that, requests are still served the cached listing at
  once while one detached background refresh revalidates it
  (stale-while-revalidate); a cancelled request never cancels a refresh. A
  request waits only while no listing has ever resolved: at startup, and
  again after each 5-second failure window while the App keeps failing.
  Concurrent waiting requests share one resolution, and a wait is bounded at
  35 seconds. Each full resolution lists the App's installations once (up
  to 10 pages of 100; more installations fail with a distinct error) and then
  makes three GitHub API calls per admitted repository: mint, read, and revoke.
  A minted token whose scope does not validate is revoked too. Revocation is
  best effort with a 2-second timeout; a failure logs one line beginning
  `github source: metadata token revocation failed:` with no token material,
  and the token still expires on its own.
- If the App resolves some admitted repositories but not others, the response
  lists those it resolved and carries their count in the
  `x-steward-unresolved-repositories` header. Only the unresolved repositories
  are retried, at most every 30 seconds. A definitive rejection (for example
  the App no longer installed on the repository, or metadata that no longer
  matches the binding) removes the repository from the listing at once. Any
  other failure keeps the previously resolved entry for up to 20 minutes since
  it last resolved, and that entry counts as unresolved in the header. A
  wholly failed refresh is retried after 30 seconds. Only when no listing has ever resolved does the
  request fail with HTTP 503, `error: "github_automation_unavailable"` and
  `reason: "source_app_unavailable"`, rather than falling back to the slower
  per-user listing; requests in the following 5 seconds share that failure
  instead of repeating it. One resolution has a 30-second deadline, and every
  repository that finished before it is kept; only those still pending count
  as unresolved. Each resolution with unresolved repositories logs one
  apiserver line beginning `admitted repository listing:` with the count and
  up to five repository IDs with their fixed reasons; no token is logged.
- A Git hosting plane that cannot describe repositories (the port's default)
  keeps the governed per-user listing for a blank query, as before. Only a
  full resolution with no listing yet can switch to that fallback; an
  Unsupported answer during a retry never discards a resolved listing.
- The governed per-user listing is unchanged for an explicit search `query`,
  and for a blank query when `githubSource` is disabled or has no bindings.
- `GithubRepositoriesResponse` gains an optional `source` field:
  `"admitted"` for the App-resolved listing and `"connection"` for the governed
  listing. The regenerated web client types include it; the browser does not
  use it yet. With the admitted listing, every repository in the Get started
  picker is **Ready**.
- Authority: listing admitted repositories to any authenticated browser user
  reveals only names, default branches, visibility and URLs that the operator
  configured as governed sources. Workflow detection, publication, dispatch and
  run status still run as governed connection operations, unchanged.
- Operations: the source GitHub App installation must include each admitted
  source repository. The App needs no new permission; the listing
  token requests only `metadata: read`. With NetworkPolicy enabled, the
  existing `networkPolicy.githubApiCidrs` egress already covers these calls.
  No migration or Helm value changes.
- Governed connection-operation reconciliation now emits one structured latency
  line when an execution attempt has both start and finish timestamps. The line
  includes `operation_kind`, `queue_wait_ms`, `attempt_duration_ms`, and
  `total_latency_ms`, separating controller/runtime activation from bridge
  execution. An unavailable timing query emits a bounded diagnostic and never
  stops reconciliation; operations that fail before an attempt starts emit no
  latency line. No scan cadence, runtime lifecycle, or metrics surface changed.

### Fixed

- Get started step 4 ("Publish the task definition") now shows the GitHub
  repository picker. Before, the only selector was inside step 3, which
  collapses once the test run succeeds. Step 4 then used an automatically
  chosen repository without showing it, and showed readiness messages about a
  repository the user never saw. Each entry shows `owner/name` and whether it
  is Ready, Not admitted, or Not ready. The first ready repository is still
  pre-selected. Steps 3 and 4 change the same selection. Steps 5 and 6
  (workflow detection and dispatch) name the selected repository, and their
  **Change repository** action returns to step 4. The selection is kept when
  onboarding evidence reloads. The not-admitted message names the repository.
  When no listed repository is admitted, the page says so: "None of your
  repositories is admitted as a governed source on this deployment; ask an
  administrator to add it."
- The GitHub repository list is now cached and shared. It uses the `swr`
  package (MIT, pinned at 2.5.1). Before, each page or component fetched the
  list again every time it mounted. Now the list loads once per page load and
  is shared by every Get started step and the task page's **Publish this task
  to GitHub** panel. Moving between them inside the app makes no new listing
  request. The list is fetched again only:
  - by **Refresh repositories** (Get started) or **Retry repositories** (after
    a failed listing);
  - by a full page load;
  - on the next visit after a failed listing or a GitHub disconnect.

  It does not refetch on window focus or reconnect, does not poll, and does not
  retry a failure automatically. As a result, after an administrator admits a
  repository, it shows as **Ready** only after **Refresh repositories** or a
  page reload. If a refresh fails, the failure and **Retry repositories**
  replace the list until a retry succeeds. API calls are unchanged.
- Repository readiness messages changed. A repository that is not ready for
  another reason now reads `owner/name is not ready for governed automation:
  <reason>.`, and step 3 no longer adds a `Not ready:` prefix.
- Restored the reserved execution transcript in the runner output archive. Steward
  0.3.11 and 0.3.12 delivered an `out/`-only archive from `GET
  /v1/tasks/{taskUid}/outputs` even when the Task requested `diagnostics.executionLog:
  full`, so steward-run 0.8.1, which requires `.steward/diagnostics/stdout.log` and
  `stderr.log` in that archive, failed every `execution-log: full` run with
  `failure-category=input-output` after the Task had succeeded. The `package-path`
  workflows Steward generates set `execution-log: full`, so **Steward 0.3.11 and 0.3.12
  break those callers; upgrade to 0.3.13.** For `full` Tasks the runner download is now
  rebuilt from the validated `out/` entries with Steward-written tar headers, so no
  agent-authored header reaches the runner, followed by both streams from the successful
  attempt's durable logs (at most 4 MiB each). The download can therefore reach the 64
  MiB stored maximum plus 8 MiB plus headers. Every other Task still receives its stored
  `out/`-only archive unchanged. The stored archive and its validation rules, the
  browser output listings and downloads, and the execution-log endpoints are unchanged,
  and agent output still cannot create or replace `.steward/diagnostics`. A `full` Task
  whose transcript is missing or over its bounds, or whose stored archive violates its
  contract, now gets a non-retryable `500` with error `task_output_delivery_failed` and
  a bounded `failureReason` instead of an archive the runner would reject. No migration
  or caller change is required.

### Upgrade and rollback

- No migrations since v0.3.12; the newest migration remains 0067. No Helm value,
  schema default, or provider-profile change is required.
- Deploy the v0.3.13 chart and all component images as one release unit.
- Rollback to v0.3.12 needs no database restore because v0.3.13 adds no schema
  or durable-state contract; restore the v0.3.12 chart and its full
  component-image set together.

## [0.3.12] - 2026-10-07

This patch fixes governed GitHub repository listing, run status, and workflow
dispatch against the pinned GitHub MCP server (`github-mcp-server` v1.6.0). It
adds no migrations and changes no Helm values.

### Changed

- A blank repository query now has a defined meaning. `GET
  /app/api/v1/github/repositories` with an empty `query` (the browser default)
  lists only repositories **owned by the authenticated GitHub user**; the bridge
  searches `user:<login>` using the login from the governed connection's
  profile. Repositories owned by an organization are not listed by default, even
  when the user can access them. Pass an explicit search query (for example
  `org:<organization>` or `repo:<owner>/<name>`) to the API to find them; the
  browser has no query field in 0.3.12. The search follows GitHub repository
  search semantics, which exclude forks by default. Earlier releases rejected the
  blank query at the bridge, so the default listing failed.
- Repository loading in Get started and on the Runs page is now independent of
  the rest of the page. The checklist and the tested package preview render
  while repositories are loading or after the listing fails. A failure shows its
  error code and bounded reason with a **Retry repositories** action.
  Publication and dispatch controls that need a repository are disabled in Get
  started, and are not shown on the Runs page, until a listing succeeds.

### Fixed

- Repository listing matches the pinned server's replies. The default
  (`user:<login>`) listing requests minimal output, whose items carry no owner
  object, so the owner's stable numeric ID comes from the `get_me` profile `id`.
  That profile ID is used only for an item with no owner object or owner ID whose
  `full_name` names the authenticated user; any other item without an owner ID is
  left out, and an item that is otherwise malformed fails the listing. Explicit
  queries, including `repo:<owner>/<name>` resolution, request full output, so
  organization-owned repositories keep their own owner ID.
- An empty search reply (`{"total_count":0}` with no `items`) is an empty list.
  Resolving a named repository that the search does not return answers HTTP 404
  instead of a provider failure.
- Run status accepts jobs nested as `{"jobs":{"total_count":N,"jobs":[...]}}` and
  treats `{"jobs":{"total_count":0}}` as no jobs. The `requested`, `waiting`, and
  `pending` run and job statuses report as `queued`. The bridge requests at most
  30 jobs (previously 100) so a run status stays within its result bound; only
  the first 30 jobs of a larger matrix run are shown.
- Workflow dispatch and run lookup pass the workflow file name (`<file>.yml`)
  rather than its full path, after checking that the workflow is directly under
  `.github/workflows/`. The `run_workflow` result is now checked: a tool error
  reporting the workflow or ref as not found (or as already existing) is a
  definite rejection reported in the `bridge-response-contract` failure category.
  It is no longer treated as a queued run, and it is not reported as an MCP-GW
  outage to retry. Other tool errors remain reported as MCP-GW unavailable.
- Bridge tests use reply envelopes captured from the pinned server for the read
  tools it parses (`get_me`, minimal and full `search_repositories`,
  `get_file_contents`, `actions_get`, `actions_list` for runs and jobs,
  `get_commit`, and `list_pull_requests`); every identifier, SHA, timestamp,
  title, body, and job name in those fixtures is synthetic.

### Failure categories, reasons, and bounds

- New connection-operation failure category `bridge-contract`: the bridge
  rejected the request Steward sent it (the invocation, the operation allowlist,
  or the `request.json` operation contract). It indicates an apiserver and bridge
  mismatch or defect, not a provider failure. The Connections API reports it as
  `bridge_contract_invalid`. An apiserver-to-bridge contract test now covers the
  default repository request.
- New connection-operation failure category `bridge_result_too_large`: the
  bridge exited successfully but its result exceeded Steward's own bound for that
  operation. It is no longer reported as `invalid_bridge_result` or as a provider
  contract failure. The Connections API reports it as `bridge_result_too_large`,
  and the Connections page explains it.
- The GitHub automation error body (`GithubAutomationErrorResponse`) has a new
  optional `reason` field. HTTP 503 `github_automation_unavailable` now carries
  `reason: "bridge_contract"`, `"bridge_response_contract"` (the provider reply
  did not satisfy the pinned contract, or the apiserver rejected the bridge
  result as `invalid_bridge_result`, including a per-field limit below), or
  `"bridge_result_too_large"`. Other 503 responses omit `reason`.
- Bridge result bounds are per operation: 128 KiB for a repository listing and
  for a run status, 32 KiB for every other operation (unchanged). These bounds
  rely on per-field limits that both the bridge and the apiserver enforce, and a
  result outside them is `invalid_bridge_result`: login and owner 39 bytes,
  repository name 100, owner and repository IDs 20 digits, default branch 255,
  URLs 255, at most 100 repositories per page; for a run status, at most 30 jobs,
  job names 500 bytes, job URLs 255, and a failure log of at most 8 KiB.
- GitHub MCP tool replies read by the bridge have their own 1 MiB bound,
  verified for replies without `Content-Length`. MCP-GW lifecycle and status
  replies, including the workflow re-run call, keep the 32 KiB bound.

### Upgrade and rollback

- No migrations since v0.3.11; the newest migration remains 0067. No Helm value,
  schema default, or provider-profile change is required.
- Deploy the v0.3.12 chart and all component images as one release unit. The
  apiserver and the Connections bridge must match: an earlier bridge rejects the
  blank repository query that the v0.3.12 apiserver and browser send. When
  `connectionsBridge.enabled=true`, update `connectionsBridge.image`,
  `connectionsBridge.sourceCommit`, `connectionsBridge.signerIdentity`, and
  `connectionsBridge.attestationBundle` to the values under "Stable bridge
  provenance inputs" in the v0.3.12 release notes.
- Rollback to v0.3.11 needs no database restore because v0.3.12 adds no schema
  or durable state; restore the v0.3.11 chart and its full component-image set,
  including the bridge coordinates, together. Operations already recorded with
  the new failure categories read as a generic unavailable failure on v0.3.11.

### Known issue

- With steward-run 0.8.1, a GitHub Actions caller that sets
  `execution-log: full` reports `failure-category=input-output` after the
  governed Task has succeeded. This includes the callers that Steward generates
  from Get started, the Task page, and the Runs page. steward-run 0.8.1 expects
  the reserved stdout and stderr transcript inside the output archive, and
  Steward 0.3.11 and later no longer put it there. The fix is tracked in #317 and
  planned for 0.3.13. Workaround: remove `execution-log: full` from the caller
  workflow. Execution logs remain available in the Steward UI and its
  owner-scoped execution-log endpoints.

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

[Unreleased]: https://github.com/apelogic-ai/steward/compare/v0.3.23...HEAD
[0.3.23]: https://github.com/apelogic-ai/steward/compare/v0.3.22...v0.3.23
[0.3.22]: https://github.com/apelogic-ai/steward/compare/v0.3.21...v0.3.22
[0.3.21]: https://github.com/apelogic-ai/steward/compare/v0.3.20...v0.3.21
[0.3.20]: https://github.com/apelogic-ai/steward/compare/v0.3.19...v0.3.20
[0.3.19]: https://github.com/apelogic-ai/steward/compare/v0.3.18...v0.3.19
[0.3.18]: https://github.com/apelogic-ai/steward/compare/v0.3.17...v0.3.18
[0.3.17]: https://github.com/apelogic-ai/steward/compare/v0.3.16...v0.3.17
[0.3.16]: https://github.com/apelogic-ai/steward/compare/v0.3.15...v0.3.16
[0.3.15]: https://github.com/apelogic-ai/steward/compare/v0.3.14...v0.3.15
[0.3.14]: https://github.com/apelogic-ai/steward/compare/v0.3.13...v0.3.14
[0.3.13]: https://github.com/apelogic-ai/steward/compare/v0.3.12...v0.3.13
[0.3.12]: https://github.com/apelogic-ai/steward/compare/v0.3.11...v0.3.12
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
