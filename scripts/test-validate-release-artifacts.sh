#!/usr/bin/env bash
# Tests the image scopes of scripts/validate-release-artifacts.sh through its
# plan, which reads the same step table the run does.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
script="${root}/scripts/validate-release-artifacts.sh"

scratch="$(mktemp -d)"
trap 'rm -rf "${scratch}"' EXIT

failures=0
fail() {
  echo "FAIL: $*" >&2
  failures=$((failures + 1))
}

# A plan must run nothing. These stand in for every tool the checks and the
# image steps start, and record any use.
mkdir "${scratch}/bin"
for tool in docker helm curl python3 cargo kind kubectl; do
  printf '#!/bin/sh\necho "%s $*" >> "%s/tool-calls"\nexit 97\n' "${tool}" "${scratch}" \
    > "${scratch}/bin/${tool}"
  chmod 0755 "${scratch}/bin/${tool}"
done

plan() {
  PATH="${scratch}/bin:${PATH}" bash "${script}" "$@" --plan
}

expect_plan() {
  local name="$1" expected="$2"
  shift 2
  local observed
  if ! observed="$(plan "$@" 2>&1)"; then
    fail "${name}: the plan failed: ${observed}"
    return
  fi
  if [[ "${observed}" != "${expected}" ]]; then
    fail "${name}: expected plan
${expected}
but got
${observed}"
  fi
}

expect_usage_error() {
  local name="$1" status=0
  shift
  PATH="${scratch}/bin:${PATH}" bash "${script}" "$@" >/dev/null 2>&1 || status="$?"
  if [[ "${status}" != 2 ]]; then
    fail "${name}: expected a usage error (exit 2), got exit ${status}"
  fi
}

# ---------------------------------------------------------------------------
# The plan of each invocation, in execution order.
# ---------------------------------------------------------------------------

checks_only='checks:release-helper-tests
checks:chart
checks:release-workflow'

full_scope='checks:release-helper-tests
checks:chart
image:apiserver
image:controller
image:mint
image:trigger-receiver
image:bridge
image:web
smoke:web
smoke:bridge
smoke:mint
smoke:trigger-receiver
checks:release-workflow'

web_scope='checks:release-helper-tests
checks:chart
image:web
smoke:web
checks:release-workflow'

expect_plan "no image build" "${checks_only}"
expect_plan "full scope by default" "${full_scope}" --build-images
expect_plan "full scope by name" "${full_scope}" --build-images --scope full
expect_plan "web scope" "${web_scope}" --build-images --scope web
expect_plan "plan flag first" "${web_scope}" --plan --build-images --scope web

# ---------------------------------------------------------------------------
# Invariants between the scopes, computed from the plans and not from the
# expectations above.
# ---------------------------------------------------------------------------

full_plan="$(plan --build-images)"
web_plan="$(plan --build-images --scope web)"
base_plan="$(plan)"

# Succeeds when every line of the first plan appears in the second, in the
# same order.
is_ordered_subset() {
  local subset="$1" superset="$2" wanted line
  local -a remaining=()
  while IFS= read -r line; do
    remaining+=("${line}")
  done <<<"${superset}"
  local index=0
  while IFS= read -r wanted; do
    while ((index < ${#remaining[@]})) && [[ "${remaining[index]}" != "${wanted}" ]]; do
      index=$((index + 1))
    done
    if ((index >= ${#remaining[@]})); then
      return 1
    fi
    index=$((index + 1))
  done <<<"${subset}"
}

is_ordered_subset "${web_plan}" "${full_plan}" ||
  fail "the full scope must cover every step of the web scope, in the same order"
# The checks that need no image are straight-line code ahead of the image
# steps, so this pins what the plan reports about them, not the code itself.
is_ordered_subset "${base_plan}" "${web_plan}" ||
  fail "the web scope must report every check that needs no image"
if is_ordered_subset "${full_plan}" "${web_plan}"; then
  fail "the web scope must be narrower than the full scope"
fi
is_ordered_subset "${full_plan}" "${full_plan}" ||
  fail "the ordered-subset check rejects an identical plan"
if is_ordered_subset 'smoke:web
image:web' "${full_plan}"; then
  fail "the ordered-subset check ignores order"
fi

# The web scope builds the web image and no other, and smoke-tests it.
web_images="$(grep '^image:' <<<"${web_plan}" || true)"
[[ "${web_images}" == image:web ]] ||
  fail "the web scope must build exactly the web image, but builds: ${web_images//$'\n'/ }"
grep -Fxq smoke:web <<<"${web_plan}" ||
  fail "the web scope must smoke-test the web image"
grep -Fxq smoke:trigger-receiver <<<"${full_plan}" ||
  fail "the full scope must smoke-test the trigger receiver image"
grep -Fq 'STEWARD_TRIGGER_BIND=127.0.0.1:0' "${script}" ||
  fail "the trigger receiver smoke must bind only an ephemeral loopback port"
grep -Fq 'the receiver release image panicked during startup' "${script}" ||
  fail "the trigger receiver smoke must reject every Rust panic"
grep -Fq 'the receiver release-image startup exceeded its deadline' "${script}" ||
  fail "the trigger receiver smoke must enforce a startup deadline"

# Every image the release publishes is built by the full scope.
release_components="$(sed -n 's/^ *for component in \(.*\); do$/\1/p' \
  "${root}/.github/workflows/release.yml" | sort -u)"
if [[ -z "${release_components}" || "${release_components}" == *$'\n'* ]]; then
  fail "the release workflow no longer names one component list"
else
  for component in ${release_components}; do
    grep -Fxq "image:${component}" <<<"${full_plan}" ||
      fail "the full scope does not build the released ${component} image"
  done
fi

# No step is a smoke test of an image its scope did not build.
for scope_plan in "${full_plan}" "${web_plan}"; do
  while IFS= read -r step; do
    case "${step}" in
      smoke:*)
        grep -Fxq "image:${step#smoke:}" <<<"${scope_plan}" ||
          fail "${step} runs in a scope that does not build its image"
        ;;
    esac
  done <<<"${scope_plan}"
done

# Every image step is dispatched to exactly one implementation, and nothing
# ends the run between the table and the dispatch loop's last step.
while IFS= read -r step; do
  case "${step}" in
    image:apiserver|image:controller|image:mint|image:trigger-receiver)
      # shellcheck disable=SC2016 # a literal line of the script under test
      arm='build_packaged_image "${image_step#image:}"'
      ;;
    image:*) arm="${step}) build_${step#image:}_image ;;" ;;
    smoke:trigger-receiver) arm='smoke:trigger-receiver) smoke_trigger_receiver_image ;;' ;;
    smoke:*) arm="${step}) smoke_${step#smoke:}_image ;;" ;;
    *) continue ;;
  esac
  [[ "$(grep -Fc -- "${arm}" "${script}")" == 1 ]] ||
    fail "${step} is not dispatched to exactly one implementation"
  case "${step}" in
    image:apiserver|image:controller|image:mint|image:trigger-receiver)
      grep -Fq "image:apiserver|image:controller|image:mint|image:trigger-receiver)" "${script}" ||
        fail "${step} has no dispatch arm"
      ;;
  esac
done <<<"${full_plan}"
grep -Fq 'trigger-receiver) binary=steward-trigger-receiver ;;' "${script}" ||
  fail "the trigger receiver image must select the exact Cargo binary name"
for implementation in build_packaged_image build_bridge_image build_web_image \
  smoke_web_image smoke_bridge_image smoke_mint_image smoke_trigger_receiver_image
do
  [[ "$(grep -c "^${implementation}() {\$" "${script}")" == 1 ]] ||
    fail "${implementation} is not defined exactly once"
done
dispatch="$(awk '/^for image_step in \$\{planned_image_steps\}; do$/ { inside = 1 } inside { print } inside && /^done$/ { exit }' "${script}")"
[[ -n "${dispatch}" ]] || fail "the image step dispatch loop could not be isolated"
if grep -Eq '(^|[[:space:];])(exit 0|break|continue|return)([[:space:];]|$)' <<<"${dispatch}"; then
  fail "the image step dispatch loop must not leave early"
fi

# ---------------------------------------------------------------------------
# Arguments. A scope is accepted only with a build, and only by name.
# ---------------------------------------------------------------------------

expect_usage_error "scope without a build" --scope web
expect_usage_error "scope without a build, planned" --scope web --plan
expect_usage_error "unknown scope" --build-images --scope docs
expect_usage_error "empty scope" --build-images --scope ''
expect_usage_error "scope without a value" --build-images --scope
expect_usage_error "scope given twice" --build-images --scope full --scope web
expect_usage_error "scope joined with an equals sign" --build-images --scope=web
expect_usage_error "unknown flag" --build-images --web
expect_usage_error "positional argument" web

if [[ -e "${scratch}/tool-calls" ]]; then
  fail "planning or rejecting arguments started a tool: $(sort -u "${scratch}/tool-calls" | tr '\n' ';')"
fi

# The script takes its scope and its build switch from its arguments only:
# each is assigned a constant default and the parsed argument, nothing else.
scope_assignments="$(grep -E '(^|[[:space:];])(image_scope|build_images|scope_given|plan_only|planned_image_steps)=' "${script}" |
  sed 's/^[[:space:]]*//' | sort)"
# shellcheck disable=SC2016 # literal lines of the script under test
expected_assignments='--build-images) build_images=1 ;;
--plan) plan_only=1 ;;
build_images=0
image_scope="$2"
image_scope=full
plan_only=0
planned_image_steps=""
planned_image_steps="$(image_steps "${image_scope}")"
scope_given=0
scope_given=1'
expected_assignments="$(sort <<<"${expected_assignments}")"
if [[ "${scope_assignments}" != "${expected_assignments}" ]]; then
  fail "the release validation script must take its scope from its arguments only, but assigns:
${scope_assignments}"
fi

# ---------------------------------------------------------------------------
# The premise of the web scope: the images it leaves out are compiled from
# the Rust workspace, and nothing there reads the browser presentation or its
# journeys. This catches a source that names one of those paths; it cannot
# see every way a build could read a file.
# ---------------------------------------------------------------------------

web_readers_status=0
web_readers="$(git -C "${root}" grep -nE '\.\./(web|tests/browser)/|"(web|tests/browser)/' -- \
  adapters bins crates migrations Cargo.toml ':!*.md')" || web_readers_status="$?"
if ((web_readers_status > 1)); then
  fail "git could not search the Rust workspace for readers of the browser presentation"
elif [[ -n "${web_readers}" ]]; then
  fail "the Rust workspace reads the browser presentation, so the web scope may no longer skip its images:
${web_readers}"
fi
for dockerfile in package connections-bridge; do
  if [[ ! -f "${root}/build/${dockerfile}.Dockerfile" ]]; then
    fail "build/${dockerfile}.Dockerfile is missing, so the premise of the web scope cannot be checked"
  elif grep -Eq '(^|[[:space:]/])(web|tests/browser)(/|[[:space:]]|$)' "${root}/build/${dockerfile}.Dockerfile"; then
    fail "build/${dockerfile}.Dockerfile names the browser presentation, so the web scope may no longer skip its image"
  fi
done

if ((failures != 0)); then
  echo "${failures} release validation scope test(s) failed" >&2
  exit 1
fi
echo 'release validation scopes passed'
