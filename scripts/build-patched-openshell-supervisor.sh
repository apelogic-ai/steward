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
APK_ARTIFACT_BASE_URL="https://dl-cdn.alpinelinux.org/alpine/v3.22/main"
APK_PACKAGE_CLOSURE=(
  "alpine-baselayout=3.7.0-r0"
  "alpine-baselayout-data=3.7.0-r0"
  "alpine-keys=2.5-r0"
  "alpine-release=3.22.5-r0"
  "apk-tools=2.14.10-r0"
  "busybox=1.37.0-r20"
  "busybox-binsh=1.37.0-r20"
  "ca-certificates-bundle=20260611-r0"
  "gmp=6.3.0-r3"
  "iptables=1.8.11-r1"
  "iptables-legacy=1.8.11-r1"
  "jansson=2.14.1-r0"
  "libapk2=2.14.10-r0"
  "libcrypto3=3.5.7-r0"
  "libip4tc=1.8.11-r1"
  "libip6tc=1.8.11-r1"
  "libmnl=1.0.5-r2"
  "libncursesw=6.5_p20250503-r0"
  "libnftnl=1.2.9-r0"
  "libssl3=3.5.7-r0"
  "libxtables=1.8.11-r1"
  "musl=1.2.5-r12"
  "musl-utils=1.2.5-r12"
  "ncurses-terminfo-base=6.5_p20250503-r0"
  "nftables=1.1.3-r0"
  "readline=8.2.13-r1"
  "scanelf=1.3.8-r1"
  "ssl_client=1.37.0-r20"
  "zlib=1.3.2-r0"
)
APK_ARTIFACTS_AARCH64=(
  "gmp-6.3.0-r3.apk=0d2eb1079b1b5692e9e6652ff0e269caeb9c812f483e34c88d461c03bcf75460"
  "iptables-1.8.11-r1.apk=0a10fe634e3525082a1219487cd044d987ac4a55ed5aa551bf758e81223e1cfb"
  "iptables-legacy-1.8.11-r1.apk=8beced6a354697e50014e2cebe0dcea3dc65d2dbbb708006a149999b3b029919"
  "jansson-2.14.1-r0.apk=f57c3bceb823add72ee0ccbf17aa7fc98596ff9eb0c11d0e5a2c28260ea87dcd"
  "libip4tc-1.8.11-r1.apk=6418c50ff5287f6aca02ba7d8baab7cfe729a898793c9a8856cf8975b3f759c2"
  "libip6tc-1.8.11-r1.apk=48ec17436d0e6fd754c8eb50862b5604e86e80ac7d19d7c7a48f9f622f305306"
  "libmnl-1.0.5-r2.apk=213a7e87553bed3d9159b2e74d2627885c259883e61714b357949ca806eb1f8d"
  "libncursesw-6.5_p20250503-r0.apk=419b375e8a4345e7172b1f0f3a3c57db61374f5408cdb875d9e860bd4c243aca"
  "libnftnl-1.2.9-r0.apk=6912a5d56b31d3365b8dd0d6339bd70a2bfc9e25dab0c387f77174113c43e664"
  "libxtables-1.8.11-r1.apk=e84f0d6b69d4318f297056d00ccbe433e6c7d163fe6767405e373248c42e3e88"
  "ncurses-terminfo-base-6.5_p20250503-r0.apk=3d37403e0b5ab9eb0c1ce269444e4a385faec9fe6af452c1c6956806b13d2bd6"
  "nftables-1.1.3-r0.apk=e48df72b87580444a6b3094e2292886994c3d9c600435d7a62949e3706ce2c07"
  "readline-8.2.13-r1.apk=334af29dbf6b5a71a87af4d6a58e2967a8f711a51d00093de0e1498daf83ceb2"
)
APK_ARTIFACTS_X86_64=(
  "gmp-6.3.0-r3.apk=d3f987ae3836ac7774324bff443dd49d03b846209660729d0c30dfff5546e138"
  "iptables-1.8.11-r1.apk=defe876173d08fe30b664b2d9f60d0237298a639e3a7d0e3f83ac637fd6519db"
  "iptables-legacy-1.8.11-r1.apk=aebfc932aa2e3b27e4895250aa4b73ead107116a3e8cd33ebf2d4036e46c043e"
  "jansson-2.14.1-r0.apk=7fde81421482507163410715a7ef7d7df6a091870edab855c24e8f6fc84e3e2d"
  "libip4tc-1.8.11-r1.apk=3ca5cd36732d59374d970d937f781469969bf668ff5eb65207892f7cfcd27f02"
  "libip6tc-1.8.11-r1.apk=dc0e2aa34b2454bce4c8f6860484925fd6e5bea8eeb24e53d4b9526a24ed4c2c"
  "libmnl-1.0.5-r2.apk=e9dc63c95a0c8a263dc7f0705e6f7a2220d632a675ce85db798d33a40b1c1b0b"
  "libncursesw-6.5_p20250503-r0.apk=aeafdfca68147b014705b4e2564639ade6345198debb522f2c6c51d32e417651"
  "libnftnl-1.2.9-r0.apk=bef674635aec00dca296b8206751245f7664ba10c416f31c01a563af596720ae"
  "libxtables-1.8.11-r1.apk=5367f1f5c309a0aeede0a08d73af717f582f7a135ff97080aa0e5b7c72fd97af"
  "ncurses-terminfo-base-6.5_p20250503-r0.apk=0815a5f0403974bb9c34d456e71dc9c0222cb5455d393bc63c44b573da3d7fe0"
  "nftables-1.1.3-r0.apk=96bdef738b0ae22ad86500af3345622c7f5bdc6ade0a407d087d1ea3223c8bc7"
  "readline-8.2.13-r1.apk=520fa586c689144928191bee13e2c85ff4e170ad87d1471ec48e3e97611673d8"
)
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
    "apk-packages=${APK_PACKAGE_CLOSURE[*]}" \
    "apk-artifact-base=${APK_ARTIFACT_BASE_URL}" \
    "apk-artifacts-aarch64=${APK_ARTIFACTS_AARCH64[*]}" \
    "apk-artifacts-x86_64=${APK_ARTIFACTS_X86_64[*]}" \
    "build-script-sha256=$(build_script_sha256)"
}

require_command() {
  local command_name=$1
  if ! command -v "${command_name}" >/dev/null 2>&1; then
    echo "missing required command: ${command_name}" >&2
    return 1
  fi
}

scrub_ambient_compiler_overrides() {
  local variable_name=""
  while IFS= read -r variable_name; do
    case "${variable_name}" in
      RUSTUP_TOOLCHAIN | \
        RUSTFLAGS | \
        RUSTDOCFLAGS | \
        RUSTC | \
        RUSTC_WRAPPER | \
        RUSTC_WORKSPACE_WRAPPER | \
        RUSTC_BOOTSTRAP | \
        CARGO_ENCODED_RUSTFLAGS | \
        CARGO_BUILD_* | CARGO_PROFILE_* | CARGO_TARGET_* | \
        CC | CC_* | *_CC | \
        CXX | CXX_* | *_CXX | \
        CPP | CPP_* | *_CPP | \
        AR | AR_* | *_AR | \
        RANLIB | RANLIB_* | *_RANLIB | \
        CFLAGS | CFLAGS_* | *_CFLAGS | \
        CXXFLAGS | CXXFLAGS_* | *_CXXFLAGS | \
        CPPFLAGS | CPPFLAGS_* | *_CPPFLAGS | \
        LDFLAGS | LDFLAGS_* | *_LDFLAGS | \
        BINDGEN_EXTRA_CLANG_ARGS | BINDGEN_EXTRA_CLANG_ARGS_* | \
        CMAKE_* | PKG_CONFIG_* | \
        SDKROOT | *_DEPLOYMENT_TARGET | \
        CPATH | C_INCLUDE_PATH | CPLUS_INCLUDE_PATH | OBJC_INCLUDE_PATH | \
        SOURCE_DATE_EPOCH)
        unset "${variable_name}"
        ;;
    esac
  done < <(compgen -v)
}

check_prerequisites() {
  local failed=0
  local actual_rust=""
  local actual_zig=""
  local actual_zigbuild=""

  for command_name in cargo curl docker git openssl rustup; do
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

build_script_sha256() {
  openssl dgst -sha256 -r "${BASH_SOURCE[0]}" | awk '{print $1}'
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

apk_architecture_for_docker_architecture() {
  case "$1" in
    arm64)
      printf 'aarch64\n'
      ;;
    amd64)
      printf 'x86_64\n'
      ;;
    *)
      echo "unsupported APK architecture for Docker architecture: $1" >&2
      return 1
      ;;
  esac
}

apk_artifact_manifest() {
  case "$1" in
    aarch64)
      printf '%s\n' "${APK_ARTIFACTS_AARCH64[@]}"
      ;;
    x86_64)
      printf '%s\n' "${APK_ARTIFACTS_X86_64[@]}"
      ;;
    *)
      echo "unsupported Alpine APK architecture: $1" >&2
      return 1
      ;;
  esac
}

download_apk_artifacts() {
  local destination=$1
  local alpine_architecture=$2
  local artifact=""
  local filename=""
  local expected_sha256=""
  local actual_sha256=""
  local download_path=""

  install -d -m 0755 "${destination}"
  while IFS= read -r artifact; do
    filename="${artifact%%=*}"
    expected_sha256="${artifact#*=}"
    download_path="${destination}/.${filename}.download"
    curl \
      --fail \
      --location \
      --proto '=https' \
      --proto-redir '=https' \
      --silent \
      --show-error \
      --tlsv1.2 \
      --output "${download_path}" \
      "${APK_ARTIFACT_BASE_URL}/${alpine_architecture}/${filename}"
    actual_sha256="$(openssl dgst -sha256 -r "${download_path}" | awk '{print $1}')"
    if [[ "${actual_sha256}" != "${expected_sha256}" ]]; then
      echo "APK checksum mismatch for ${filename}: got ${actual_sha256}, expected ${expected_sha256}" >&2
      return 1
    fi
    mv "${download_path}" "${destination}/${filename}"
  done < <(apk_artifact_manifest "${alpine_architecture}")
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
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

scrub_ambient_compiler_overrides

export GIT_CONFIG_GLOBAL=/dev/null
export GIT_CONFIG_SYSTEM=/dev/null
export GIT_CONFIG_NOSYSTEM=1

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
apk_architecture="$(apk_architecture_for_docker_architecture "${architecture}")"
stage="${SOURCE_DIR}/deploy/docker/.build/prebuilt-binaries/${architecture}"
apk_stage="${SOURCE_DIR}/deploy/docker/.build/steward-apks"
binary="${SUPERVISOR_TARGET_DIR}/${rust_target}/release/openshell-sandbox"
upstream_dockerfile="${SOURCE_DIR}/deploy/docker/Dockerfile.supervisor"
pinned_dockerfile="${RUN_DIR}/Dockerfile.supervisor.pinned"

download_apk_artifacts "${apk_stage}" "${apk_architecture}"

install -d -m 0700 "${SUPERVISOR_CARGO_HOME}"
install \
  -m 0644 \
  "${SOURCE_DIR}/.cargo/config.toml" \
  "${SUPERVISOR_CARGO_HOME}/config.toml"

(
  cd "${SOURCE_DIR}"
  export PATH="${CROSS_TOOL_PATH}"
  scrub_ambient_compiler_overrides
  source tasks/scripts/build-env.sh
  ensure_build_nofile_limit
  rustup target add --toolchain "${RUST_TOOLCHAIN}" "${rust_target}"
  cd /
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
      --manifest-path "${SOURCE_DIR}/Cargo.toml" \
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
      echo "COPY deploy/docker/.build/steward-apks/ /tmp/steward-apks/"
      echo "RUN --network=none apk --no-cache --no-network --repositories-file /dev/null add /tmp/steward-apks/*.apk && rm -rf /tmp/steward-apks"
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
