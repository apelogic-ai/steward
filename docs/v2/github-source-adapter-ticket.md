# DP-G01: Provider-neutral Git source port and GitHub adapter

Priority: P0

Status: design-audited; production edits wait for DP-C01 and dependency approval

## Goal

Let Steward retrieve exact Git objects from admitted repositories without trusting
workflow-uploaded package bytes or caller-supplied credentials.

## Scope

- evolve the existing provisional `GitHostingPlane` into a Steward-domain read port
  using stable repository identity, exact commit, repository-relative path, and an
  explicit byte bound rather than adding another Git port;
- implement the first adapter with a read-only GitHub App;
- mint short-lived installation tokens server-side;
- map canonical clone URLs to stable GitHub repository and owner IDs;
- retrieve exact files at exact commits and revalidate stable repository identity on
  every read;
- enforce bounded responses and per-file object size;
- reject traversal, symlinks, submodules, mutable refs, redirects, ambiguous tree
  entries, and inconsistent commit, tree, or blob reads; and
- return deterministic content plus provider-neutral provenance to DP-S01.

The workflow `GITHUB_TOKEN`, PATs, deploy keys, and uploaded source archives are not
accepted by this boundary.

## Negative tests

- an App not installed on the declared repository fails closed;
- a repository name resolving to the wrong stable ID is rejected;
- missing, replaced, or inconsistent objects cannot produce a successful read;
- path and symlink escape attempts cannot read outside the declared repository tree;
- caller credentials are ignored and never forwarded; and
- App installation removal or a changed stable repository identity prevents new reads
  even when old Git objects remain available.

DP-S01, not this vendor adapter, owns package parsing, dependency traversal, duplicate
and cycle rejection, cross-source policy, total closure size/count/deadline,
caller-to-source authorization, canonicalization, and the closure digest.

## Exit criteria

- unit and adapter integration tests cover exact same-repository reads and two
  independently installed repositories without allowing identity confusion;
- returned bytes are bound to stable repository identity and exact commit;
- installation tokens and provider responses do not enter logs or evidence; and
- the port admits future GitLab, Gitea, or generic Git implementations without GitHub
  types leaking into Steward core.

## Parallel boundary

May proceed alongside DP-I01, DP-R01, and DP-A01 after DP-C01. DP-S01 depends on the
port and adapter. The initial DP-S01 package path remains limited to the invoking
repository at its exact triggered commit; transport support for multiple admitted
repositories does not enable a cross-repository package closure. Do not use local-main
for adapter development.

## Planned dependency boundary

The adapter crate adds no new third-party version. Its proposed direct dependencies
are the already workspace-pinned `base64`, `jsonwebtoken`, `reqwest`, `serde`, and
`serde_json`, plus internal `steward-ports` and `steward-types`; loopback integration
tests use the already pinned `tokio` as a dev-dependency. Production edits require the
maintainer's advance dependency approval.
