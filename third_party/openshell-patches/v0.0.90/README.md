# OpenShell v0.0.90 supervisor identity patch

Base: OpenShell `v0.0.90`
(`1d4ac708f1d2a9ab94204cdce6ca0eee7e792839`).

## Why it is carried

With `provider_spiffe_workload_api_socket_path` configured, the Kubernetes
driver mounts the SPIFFE Workload API socket and sets
`OPENSHELL_PROVIDER_SPIFFE_WORKLOAD_API_SOCKET`. The process supervisor later
requires a prepared child mount namespace so the socket stays outside the
agent's view, but the preparation function is never called. A sandbox therefore
fails before readiness with:

```text
supervisor identity mount namespace was not prepared before startup hardening
```

The helper is Linux-gated and public, so the compiler does not report it as dead
code. It is not reached through a trait or macro. The patch calls it during
privileged process startup, before supervisor seccomp hardening, and preserves
the existing fail-closed behavior if identity isolation cannot be established.

## Upstream attempt and exit condition

- [NVIDIA/OpenShell#2012](https://github.com/NVIDIA/OpenShell/pull/2012) proposed
  the same focused wiring fix and was automatically closed by the first-time
  contributor vouch gate.
- [NVIDIA/OpenShell#2184](https://github.com/NVIDIA/OpenShell/pull/2184) remains
  open and also wires the call, but combines it with broader recovery behavior
  that continues without supervisor identity isolation.
- A focused branch is prepared at
  `lbelyaev/OpenShell:2184-prepare-supervisor-identity-namespace/lbelyaev`.
  Upstream will auto-close its PR until the human contributor completes the
  required vouch process.

Remove this patch when a NVIDIA/OpenShell PR containing the equivalent
fail-closed startup call merges and Steward pins a release that contains it.
Apply it only to the recorded base with
`git apply --unidiff-zero 0001-prepare-supervisor-identity-mount-namespace.patch`;
the zero-context form keeps the patch artifact free of whitespace-only context
lines while the exact base commit supplies the safety boundary.

## Verification

- Red: the unpatched v0.0.90 SPIFFE token-grant demo crash-loops before the
  sandbox can obtain a JWT-SVID.
- Focused upstream checks: `cargo fmt --all -- --check`,
  `cargo test -p openshell-supervisor-process` (80 passed; one privileged test
  ignored by upstream), and
  `cargo clippy -p openshell-supervisor-process --all-targets -- -D warnings`.
- Full upstream pre-commit and the patched live demo remain required before the
  upstream branch is pushed.
