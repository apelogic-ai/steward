# DP-G01: Provider-neutral Git source port and GitHub adapter

Priority: P0

Status: blocked only on DP-C01 contract freeze

## Goal

Let Steward retrieve exact Git objects from admitted repositories without trusting
workflow-uploaded package bytes or caller-supplied credentials.

## Scope

- define a Steward-domain Git source port using stable repository identity, exact
  commit, and repository-relative path;
- implement the first adapter with a read-only GitHub App;
- mint short-lived installation tokens server-side;
- map canonical clone URLs to stable GitHub repository and owner IDs;
- retrieve the invocation manifest and package closure at exact commits;
- enforce bounded object size, total closure size, dependency count, and fetch time;
- reject traversal, symlink escape, mutable refs, cross-source dependencies, and
  inconsistent exact-object reads; and
- return deterministic content plus provider-neutral provenance to DP-S01.

The workflow `GITHUB_TOKEN`, PATs, deploy keys, and uploaded source archives are not
accepted by this boundary.

## Negative tests

- an App not installed on the declared repository fails closed;
- a repository name resolving to the wrong stable ID is rejected;
- missing, replaced, or inconsistent objects cannot produce a closure;
- path and symlink escape attempts cannot read outside the declared repository tree;
- caller credentials are ignored and never forwarded; and
- source authorization revocation prevents new resolutions even when old Git objects
  remain available.

## Exit criteria

- unit and adapter integration tests cover same-repository and authorized
  cross-repository reads;
- returned bytes are bound to stable repository identity and exact commit;
- installation tokens and provider responses do not enter logs or evidence; and
- the port admits future GitLab, Gitea, or generic Git implementations without GitHub
  types leaking into Steward core.

## Parallel boundary

May proceed alongside DP-I01, DP-R01, and DP-A01 after DP-C01. DP-S01 depends on the
port and adapter. Do not use local-main for adapter development.
