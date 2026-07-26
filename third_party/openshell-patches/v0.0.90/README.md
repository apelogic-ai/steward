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
- The focused, signed-off fix is
  [`lbelyaev/OpenShell@151f6ba6`](https://github.com/lbelyaev/OpenShell/commit/151f6ba6)
  on branch `2184-prepare-supervisor-identity-namespace/lbelyaev`. Upstream
  will auto-close its PR until the human contributor completes the required
  vouch process.

Remove this patch when a NVIDIA/OpenShell PR containing the equivalent
fail-closed startup call merges and Steward pins a release that contains it.
Apply it only to the recorded base with
`git apply 0001-prepare-supervisor-identity-mount-namespace.patch`.
The patch carries surrounding startup context so it refuses to apply if the
privileged-setup and hardening boundary moves.

## Verification

- Red: the unpatched v0.0.90 SPIFFE token-grant demo crash-loops before the
  sandbox can obtain a JWT-SVID.
- Focused upstream checks: `cargo fmt --all -- --check`,
  `cargo test -p openshell-supervisor-process` (80 passed; one privileged test
  ignored by upstream), and
  `cargo clippy -p openshell-supervisor-process --all-targets -- -D warnings`.
- Full upstream `mise run pre-commit` passed on the focused current-main branch.
- Live against the exact patched v0.0.90 base: the sandbox reached readiness,
  the `ClusterSPIFFEID` selected it using `openshell.io/sandbox-id`, and the
  supervisor obtained a JWT-SVID and sent it to the profile's configured token
  endpoint. With SPIRE's published X.509 bundle mounted as the Node issuer's
  trust anchor, the upstream demo completed audience-specific exchanges for
  both protected services and preserved the sandbox SPIFFE ID as `azp`.

Build the image from the recorded commit and patch with Rust 1.95.0, Zig
0.14.1, and cargo-zigbuild 0.22.3:

```bash
scripts/build-patched-openshell-supervisor.sh
```

The script clones the immutable commit into `.steward-run/`, refuses a patch
context mismatch, isolates Cargo and compiler configuration, cross-builds the
Linux supervisor, and packages it with a digest-pinned Dockerfile frontend,
base image, and a version-locked closure of every installed APK package.
Packages absent from the base image are downloaded by exact artifact name,
verified against architecture-specific SHA-256 digests, and installed with
APK networking disabled; the runtime build never resolves a mutable
`APKINDEX`. The cache contract includes the build script itself, so
output-affecting build logic invalidates an older image. The script removes the
source tree unconditionally.
`STEWARD_DEV_KEEP=1` retains that source for debugging and prints its exact
cleanup command.

The identity spike reuses this image only when its complete declared build
contract (including the pinned cross-toolchain), patch content digest, and
architecture match; otherwise it rebuilds it automatically. It then loads the
image only into its ephemeral kind cluster. An explicit image override remains
available for upstream-fix verification:

```bash
STEWARD_OPENSHELL_SUPERVISOR_IMAGE=openshell/supervisor:steward-spiffe-v0090 \
  scripts/s0-0-openshell-spike.sh
```
