# Steward GitHub Actions generator contract

Status: renderer core v1; browser integration is a separate change

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
header. It is generation-time provenance; the service envelope remains the runtime authority and
is enforced again when Steward admits the Task.

The v1 request schema is `steward/github-actions-render-request/v1`. Its only task template is
`github-file-read/v1`, with a repository name, full 40-character Git commit, and relative file
path. Commands, runners, images, permissions, action references, environment expressions, secret
references, and arbitrary YAML are not request fields.

## Frozen steward-run release

The first reviewed contract is steward-run v0.3.7. Its signed release-manifest schema 4 records:

| Coordinate | Immutable value |
|---|---|
| Reusable workflow | `apelogic-ai/steward-run/.github/workflows/steward-task.yml@9c7487bd18d5e90b24b3e4b296bfdd232a3f4f5a` |
| Remote action | `apelogic-ai/steward-run@b26790e29ce9c243c6a7aa00450a2a1a98fbd250` |
| Governed job container | `663383948333.dkr.ecr.us-east-1.amazonaws.com/steward-run@sha256:27235891b596debb1d8bba5f7763e14a56ce4435e2fc82f3de80122b19ff8c61` |

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
Changing it requires updating the renderer tests and rechecking the corresponding steward-run
signed release contract. No live workflow is dispatched by this renderer slice.
