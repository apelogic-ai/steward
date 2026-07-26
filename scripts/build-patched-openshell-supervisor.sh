#!/usr/bin/env bash

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SOURCE_URL="https://github.com/NVIDIA/OpenShell.git"
SOURCE_COMMIT="1d4ac708f1d2a9ab94204cdce6ca0eee7e792839"
PATCH_RELATIVE="third_party/openshell-patches/v0.0.90/0001-prepare-supervisor-identity-mount-namespace.patch"
PATCH_PATH="${ROOT}/${PATCH_RELATIVE}"
IMAGE="openshell/supervisor:steward-spiffe-v0090"
RUST_TOOLCHAIN="1.95.0"
ZIG_VERSION="0.14.1"
CARGO_ZIGBUILD_VERSION="0.22.3"
DOCKERFILE_FRONTEND_IMAGE="docker/dockerfile:1.4@sha256:9ba7531bd80fb0a858632727cf7a112fbfd19b17e94c4e84ced81e24ef1a0dbc"
SUPERVISOR_BASE_IMAGE="alpine:3.22@sha256:14358309a308569c32bdc37e2e0e9694be33a9d99e68afb0f5ff33cc1f695dce"
NFTABLES_VERSION="1.1.3-r0"
IPTABLES_VERSION="1.8.11-r1"
IPTABLES_LEGACY_VERSION="1.8.11-r1"
CROSS_TOOL_PATH=""

print_contract() {
  printf '%s\n' \
    "source=${SOURCE_URL}" \
    "commit=${SOURCE_COMMIT}" \
    "patch=${PATCH_RELATIVE}" \
    "image=${IMAGE}" \
    "rust=${RUST_TOOLCHAIN}" \
    "zig=${ZIG_VERSION}" \
    "cargo-zigbuild=${CARGO_ZIGBUILD_VERSION}" \
    "dockerfile-frontend=${DOCKERFILE_FRONTEND_IMAGE}" \
    "supervisor-base=${SUPERVISOR_BASE_IMAGE}" \
    "nftables=${NFTABLES_VERSION}" \
    "iptables=${IPTABLES_VERSION}" \
    "iptables-legacy=${IPTABLES_LEGACY_VERSION}"
}

require_command() {
  local command_name=$1
  if ! command -v "${command_name}" >/dev/null 2>&1; then
    echo "missing required command: ${command_name}" >&2
    return 1
  fi
}

check_prerequisites() {
  local failed=0
  local actual_rust=""
  local actual_zig=""
  local actual_zigbuild=""

  for command_name in cargo docker git openssl rustup; do
    if ! require_command "${command_name}"; then
      failed=1
    fi
  done
  if command -v mise >/dev/null 2>&1 \
    && mise where "zig@${ZIG_VERSION}" >/dev/null 2>&1 \
    && mise where "github:rust-cross/cargo-zigbuild@${CARGO_ZIGBUILD_VERSION}" >/dev/null 2>&1
  then
    CROSS_TOOL_PATH="$(
      printf '%s:%s:%s' \
        "$(mise where "zig@${ZIG_VERSION}")" \
        "$(mise where "github:rust-cross/cargo-zigbuild@${CARGO_ZIGBUILD_VERSION}")" \
        "${PATH}"
    )"
  elif command -v zig >/dev/null 2>&1 && command -v cargo-zigbuild >/dev/null 2>&1; then
    CROSS_TOOL_PATH="${PATH}"
  else
    echo "zig ${ZIG_VERSION} and cargo-zigbuild ${CARGO_ZIGBUILD_VERSION} are required" >&2
    failed=1
  fi

  if [[ "${failed}" == "0" ]]; then
    if ! actual_rust="$(rustup run "${RUST_TOOLCHAIN}" rustc --version 2>/dev/null)"; then
      echo "Rust ${RUST_TOOLCHAIN} is required through rustup" >&2
      failed=1
    elif [[ "$(printf '%s\n' "${actual_rust}" | awk '{print $2}')" != "${RUST_TOOLCHAIN}" ]]; then
      echo "Rust ${RUST_TOOLCHAIN} is required; found ${actual_rust}" >&2
      failed=1
    fi
    actual_zig="$(env PATH="${CROSS_TOOL_PATH}" zig version)"
    actual_zigbuild="$(
      env PATH="${CROSS_TOOL_PATH}" cargo-zigbuild --version | awk '{print $2}'
    )"
    if [[ "${actual_zig}" != "${ZIG_VERSION}" ]]; then
      echo "zig ${ZIG_VERSION} is required; found ${actual_zig}" >&2
      failed=1
    fi
    if [[ "${actual_zigbuild}" != "${CARGO_ZIGBUILD_VERSION}" ]]; then
      echo "cargo-zigbuild ${CARGO_ZIGBUILD_VERSION} is required; found ${actual_zigbuild}" >&2
      failed=1
    fi
  fi
  if command -v docker >/dev/null 2>&1; then
    if ! docker buildx version >/dev/null 2>&1; then
      echo "docker buildx is required" >&2
      failed=1
    fi
    if [[ "$(docker info --format '{{.OSType}}' 2>/dev/null || true)" != "linux" ]]; then
      echo "a running Linux Docker engine is required" >&2
      failed=1
    fi
  fi
  if [[ ! -f "${PATCH_PATH}" ]]; then
    echo "carried patch is missing: ${PATCH_RELATIVE}" >&2
    failed=1
  fi
  if [[ "${failed}" != "0" ]]; then
    return 1
  fi
}

patch_sha256() {
  openssl dgst -sha256 -r "${PATCH_PATH}" | awk '{print $1}'
}

build_contract_sha256() {
  {
    print_contract
    printf 'patch-sha256=%s\n' "$(patch_sha256)"
  } | openssl dgst -sha256 | awk '{print $NF}'
}

docker_architecture() {
  case "$(docker info --format '{{.Architecture}}')" in
    aarch64 | arm64)
      printf 'arm64\n'
      ;;
    x86_64 | amd64)
      printf 'amd64\n'
      ;;
    *)
      echo "unsupported Docker architecture" >&2
      return 1
      ;;
  esac
}

image_is_current() {
  local actual_metadata=""
  local expected_metadata=""

  require_command docker
  require_command openssl
  if [[ ! -f "${PATCH_PATH}" ]]; then
    echo "carried patch is missing: ${PATCH_RELATIVE}" >&2
    return 1
  fi
  expected_metadata="$(
    printf '%s|%s' \
      "$(build_contract_sha256)" \
      "$(docker_architecture)"
  )"
  actual_metadata="$(
    docker image inspect \
      --format '{{ index .Config.Labels "io.apelogic.steward.build-contract-sha256" }}|{{.Architecture}}' \
      "${IMAGE}" 2>/dev/null
  )" || true
  if [[ "${actual_metadata}" != "${expected_metadata}" ]]; then
    echo "${IMAGE} does not match the pinned build contract and Docker architecture" >&2
    return 1
  fi
}

rust_target_for_architecture() {
  case "$1" in
    arm64)
      printf 'aarch64-unknown-linux-musl\n'
      ;;
    amd64)
      printf 'x86_64-unknown-linux-musl\n'
      ;;
    *)
      echo "unsupported build architecture: $1" >&2
      return 1
      ;;
  esac
}

if [[ "${1:-}" == "--print-contract" ]]; then
  if [[ "$#" != "1" ]]; then
    echo "--print-contract takes no additional arguments" >&2
    exit 2
  fi
  print_contract
  exit 0
fi

if [[ "${1:-}" == "--check-prerequisites" ]]; then
  if [[ "$#" != "1" ]]; then
    echo "--check-prerequisites takes no additional arguments" >&2
    exit 2
  fi
  check_prerequisites
  echo "patched OpenShell supervisor build prerequisites are present"
  exit 0
fi

if [[ "${1:-}" == "--image-is-current" ]]; then
  if [[ "$#" != "1" ]]; then
    echo "--image-is-current takes no additional arguments" >&2
    exit 2
  fi
  image_is_current
  exit 0
fi

if [[ "$#" != "0" ]]; then
  echo "usage: $0 [--print-contract|--check-prerequisites|--image-is-current]" >&2
  exit 2
fi

check_prerequisites

mkdir -p "${ROOT}/.steward-run"
RUN_DIR="$(mktemp -d "${ROOT}/.steward-run/openshell-supervisor-build.XXXXXX")"
SOURCE_DIR="${RUN_DIR}/OpenShell"
SUPERVISOR_CARGO_HOME="${RUN_DIR}/cargo-home"
SUPERVISOR_TARGET_DIR="${RUN_DIR}/cargo-target"

cleanup() {
  if [[ "${STEWARD_DEV_KEEP:-0}" == "1" ]]; then
    echo "kept patched OpenShell source at ${RUN_DIR}" >&2
    echo "cleanup: rm -rf -- ${RUN_DIR}" >&2
    return
  fi
  if [[ "${RUN_DIR}" == "${ROOT}/.steward-run/"* ]]; then
    rm -rf -- "${RUN_DIR}"
  else
    echo "refusing to remove unexpected build directory: ${RUN_DIR}" >&2
  fi
}
trap cleanup EXIT INT TERM

git init --quiet "${SOURCE_DIR}"
git -C "${SOURCE_DIR}" remote add origin "${SOURCE_URL}"
git -C "${SOURCE_DIR}" fetch --quiet --depth=1 origin "${SOURCE_COMMIT}"
git -C "${SOURCE_DIR}" checkout --quiet --detach FETCH_HEAD

actual_commit="$(git -C "${SOURCE_DIR}" rev-parse HEAD)"
if [[ "${actual_commit}" != "${SOURCE_COMMIT}" ]]; then
  echo "OpenShell source resolved to ${actual_commit}, expected ${SOURCE_COMMIT}" >&2
  exit 1
fi

git -C "${SOURCE_DIR}" apply --check "${PATCH_PATH}"
git -C "${SOURCE_DIR}" apply "${PATCH_PATH}"
git -C "${SOURCE_DIR}" diff --check
git -C "${SOURCE_DIR}" apply --reverse --check "${PATCH_PATH}"

architecture="$(docker_architecture)"
rust_target="$(rust_target_for_architecture "${architecture}")"
stage="${SOURCE_DIR}/deploy/docker/.build/prebuilt-binaries/${architecture}"
binary="${SUPERVISOR_TARGET_DIR}/${rust_target}/release/openshell-sandbox"
upstream_dockerfile="${SOURCE_DIR}/deploy/docker/Dockerfile.supervisor"
pinned_dockerfile="${RUN_DIR}/Dockerfile.supervisor.pinned"

(
  cd "${SOURCE_DIR}"
  export PATH="${CROSS_TOOL_PATH}"
  unset RUSTUP_TOOLCHAIN RUSTFLAGS CARGO_ENCODED_RUSTFLAGS
  unset RUSTC RUSTC_WRAPPER RUSTC_WORKSPACE_WRAPPER RUSTDOCFLAGS RUSTC_BOOTSTRAP
  unset CARGO_BUILD_RUSTC CARGO_BUILD_RUSTFLAGS CARGO_BUILD_TARGET_DIR
  unset CARGO_PROFILE_RELEASE_CODEGEN_UNITS CARGO_PROFILE_RELEASE_DEBUG
  unset CARGO_PROFILE_RELEASE_LTO CARGO_PROFILE_RELEASE_OPT_LEVEL
  unset CARGO_PROFILE_RELEASE_PANIC CARGO_PROFILE_RELEASE_STRIP
  source tasks/scripts/build-env.sh
  ensure_build_nofile_limit
  rustup target add --toolchain "${RUST_TOOLCHAIN}" "${rust_target}"
  CARGO_HOME="${SUPERVISOR_CARGO_HOME}" \
    CARGO_TARGET_DIR="${SUPERVISOR_TARGET_DIR}" \
    CARGO_INCREMENTAL=0 \
    CARGO_ZIGBUILD_CACHE_DIR="${RUN_DIR}/cargo-zigbuild-cache" \
    ZIG_GLOBAL_CACHE_DIR="${RUN_DIR}/zig-global-cache" \
    ZIG_LOCAL_CACHE_DIR="${RUN_DIR}/zig-local-cache" \
    cargo +"${RUST_TOOLCHAIN}" zigbuild \
      --locked \
      --release \
      --target "${rust_target}" \
      -p openshell-sandbox \
      --bin openshell-sandbox
)

mkdir -p "${stage}"
install -m 0755 "${binary}" "${stage}/openshell-sandbox"

frontend_replacements=0
base_replacements=0
package_replacements=0
while IFS= read -r line; do
  case "${line}" in
    "# syntax=docker/dockerfile:1.4")
      echo "# syntax=${DOCKERFILE_FRONTEND_IMAGE}"
      frontend_replacements=$((frontend_replacements + 1))
      ;;
    "FROM alpine:3.22 AS supervisor")
      echo "FROM ${SUPERVISOR_BASE_IMAGE} AS supervisor"
      base_replacements=$((base_replacements + 1))
      ;;
    "RUN apk add --no-cache nftables iptables iptables-legacy")
      echo "RUN apk add --no-cache nftables=${NFTABLES_VERSION} iptables=${IPTABLES_VERSION} iptables-legacy=${IPTABLES_LEGACY_VERSION}"
      package_replacements=$((package_replacements + 1))
      ;;
    *)
      printf '%s\n' "${line}"
      ;;
  esac
done <"${upstream_dockerfile}" >"${pinned_dockerfile}"
if [[ "${frontend_replacements}" != "1" || "${base_replacements}" != "1" || "${package_replacements}" != "1" ]]; then
  echo "OpenShell supervisor Dockerfile changed; rebase the pinned runtime inputs" >&2
  exit 1
fi

docker buildx build \
  --platform "linux/${architecture}" \
  --file "${pinned_dockerfile}" \
  --target supervisor \
  --tag "${IMAGE}" \
  --label "org.opencontainers.image.revision=${SOURCE_COMMIT}" \
  --label "io.apelogic.steward.patch=${PATCH_RELATIVE}" \
  --label "io.apelogic.steward.patch-sha256=$(patch_sha256)" \
  --label "io.apelogic.steward.build-contract-sha256=$(build_contract_sha256)" \
  --provenance=false \
  --load \
  "${SOURCE_DIR}"

image_id="$(docker image inspect --format '{{.Id}}' "${IMAGE}")"
echo "built ${IMAGE} from ${SOURCE_COMMIT} (${image_id})"
