# Steward GitHub Actions generator contract

Status: current Steward v0.3.6 renderer and browser integration

The generator turns one authoritative Steward envelope selection and one bounded task template
into workflow YAML that a developer may inspect, copy, commit, and dispatch. It never selects a
repository, writes to GitHub, requests a credential, or accepts arbitrary workflow source.

## Trust boundary

`GithubActionsRenderRequest` is untrusted input. `GithubActionsRenderContext` is constructed by
the authenticated server from:

- the currently authoritative envelope ID, monotonically increasing revision, and SHA-256 digest;
- a steward-run release whose signed release manifest has already been verified; and
- the task-template IDs whose computed authority fits inside that envelope.

The renderer requires the request and context bindings to match exactly. A stale envelope, a
different release, a template outside the envelope, an unknown field, or an unsupported schema
fails before YAML is emitted. The envelope binding is repeated in the non-secret generated-file
header. It is generation-time provenance; Task admission independently resolves and snapshots the
authenticated user's exact provisioned User Envelope. The renderer and deployment capability
catalog grant no runtime authority.

The lower-level v1 renderer accepts schema
`steward/github-actions-render-request/v1`. Its only task template is
`github-file-read/v1`, with a repository name, full 40-character Git commit,
and relative file path. Commands, runners, images, permissions, action
references, environment expressions, secret references, and arbitrary YAML
are not request fields.

The browser route accepts a server-published versioned Workflow reference such
as `repo-summary@2`. It resolves the Workflow and the user's provisioned
Envelope on the server and emits schema
`steward/github-actions-rendered-workflow/v2`. Both render paths forward the
authoritative Envelope digest to steward-run as `envelope-digest`; neither path
lets the caller supply release coordinates or authority.

The reusable workflow uploads only the governed task's `out/` directory and
treats an empty directory as an error. A published Workflow prompt must direct
the agent to write every returned result beneath `out/`. The reserved
`repo-summary@2` sample writes its Markdown result to `out/summary.md`.

## BOM-selected steward-run release

Steward source does not select a steward-run release. Release/integration
packaging verifies steward-run's signed release handoff, records its exact
coordinates in the installation BOM, and supplies that object through
`config.apiserver.stewardRunRelease`. The object contains OSS release-manifest
schema 3, semantic version, reusable-workflow repository and commit, action commit, and
governed job-container digest. The generator requires structurally immutable
coordinates and steward-run v0.7.0 or later.

The committed golden workflow uses reserved example coordinates. It tests
deterministic rendering and is not an installation BOM or a deployable pin.

The reusable workflow, not the caller, pins the remote action and owns the six-operation Task
lifecycle and unconditional finalization. The generated caller retains only `contents: read` and
`id-token: write` on the governed job. Seed and verification jobs have only `contents: read`, run
inside the same immutable container on the configured ARC runner, and use full-SHA artifact action
references. There is no checkout, PAT, GitHub App token, deploy key, long-lived bearer token, or
caller-selected job container.

Repository administrators provide these non-secret Actions variables:

- `STEWARD_RUNNER_LABEL`
- `STEWARD_API_URL`
- `IDENTITY_EXCHANGE_URL`
- `STEWARD_CA_CERTIFICATE_FILE`

The generator never resolves or persists their values.

## Determinism and validation

For identical request and authoritative context, rendering is byte-identical. The response uses
schema `steward/github-actions-rendered-workflow/v1`, content type `application/yaml`, and includes
the SHA-256 digest of the exact bytes.

`validate_generated_github_actions_yaml` applies bounded parsing, rejects duplicate mapping keys,
anchors, aliases, merge keys, multiple documents, odd indentation, and resource-amplifying input,
then requires exact equality with the canonical render. JSON request deserialization denies
unknown and duplicate members. User-supplied fields use narrow allowlists and reject control
characters, traversal, mutable revisions, and secret-like markers.

The golden contract is
[`github-file-read-v1.yaml`](examples/github-file-read-v1.yaml).
Changing the renderer shape requires updating its tests. Changing deployment
coordinates requires a newly verified installation BOM, not a Steward source
change. No live workflow is dispatched by this renderer slice.
