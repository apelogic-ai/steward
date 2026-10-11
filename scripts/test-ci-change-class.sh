#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
script="${root}/scripts/ci-change-class.sh"
# shellcheck source=scripts/ci-change-class.sh
source "${script}"

scratch="$(mktemp -d)"
trap 'rm -rf "${scratch}"' EXIT

failures=0
fail() {
  echo "FAIL: $*" >&2
  failures=$((failures + 1))
}

# ---------------------------------------------------------------------------
# classify: a table of change sets and the class each must produce.
# An entry is "<status> <old-mode> <new-mode> <path>"; a bare path is a
# modified regular file.
# ---------------------------------------------------------------------------

zero=0000000000000000000000000000000000000000
blob=1111111111111111111111111111111111111111

raw_record() {
  local status="$1" old_mode="$2" new_mode="$3" path="$4"
  printf ':%s %s %s %s %s\0%s\0' "${old_mode}" "${new_mode}" "${zero}" "${blob}" "${status}" "${path}"
}

raw_entries() {
  local entry status old_mode new_mode path
  for entry in "$@"; do
    case "${entry}" in
      [ADMTRU]\ [0-9]*)
        read -r status old_mode new_mode path <<<"${entry}"
        raw_record "${status}" "${old_mode}" "${new_mode}" "${path}"
        ;;
      *) raw_record M 100644 100644 "${entry}" ;;
    esac
  done
}

expect_class() {
  local expected="$1" name="$2"
  shift 2
  local observed
  observed="$(raw_entries "$@" | bash "${script}" classify 2>/dev/null)"
  if [[ "${observed}" != "${expected}" ]]; then
    fail "${name}: expected ${expected}, got ${observed}"
  fi
}

# docs: every path is on the documentation allowlist.
expect_class docs "root agent instructions only" AGENTS.md
expect_class docs "root CLAUDE.md" CLAUDE.md
expect_class docs "nested agent instructions" conformance/AGENTS.md crates/steward-mint/CLAUDE.md
expect_class docs "agent definition" .claude/agents/x.md
expect_class docs "skill definition" .claude/skills/review/SKILL.md
expect_class docs "codex skill definition" .codex/skills/review/SKILL.md
expect_class docs "root README" README.md
expect_class docs "READMEs below source roots" crates/steward-mint/README.md adapters/openshell/README.md bins/steward-apiserver/README.md e2e/README.md xtask/README.md conformance/README.md docs/v2/README.md
expect_class docs "changelog only" CHANGELOG.md
expect_class docs "prose under docs" docs/README.md docs/v2/overview.md docs/installation/upgrade-v0.3.0.md
expect_class docs "static image under docs" docs/diagrams/overview.png
expect_class docs "added and deleted prose" "A 000000 100644 docs/new.md" "D 100644 000000 docs/old.md"
expect_class docs "prose renamed inside docs" "D 100644 000000 docs/a.md" "A 000000 100644 docs/b.md"

# web: every path is under web/ or tests/browser/, or is documentation.
expect_class web "web source" web/src/app/page.tsx
expect_class web "browser journey" tests/browser/steward-next.spec.mjs
expect_class web "web manifest" web/package.json
expect_class web "generated client under web" web/src/api-client/types.gen.ts
expect_class web "web with documentation" web/src/app/page.tsx docs/README.md AGENTS.md
expect_class web "Markdown under web is web, not docs" web/README.md web/AGENTS.md
expect_class web "executable web script" "A 000000 100755 web/scripts/build.sh"
expect_class web "deleted web file" "D 100644 000000 web/src/old.tsx"

# full: anything else.
expect_class full "Rust source" crates/steward-types/src/lib.rs
expect_class full "deleted Rust file" "D 100644 000000 crates/steward-types/src/old.rs"
expect_class full "web plus a deleted Rust file" web/src/app/page.tsx "D 100644 000000 crates/steward-types/src/old.rs"
expect_class full "rename from web to crates" "D 100644 000000 web/src/util.ts" "A 000000 100644 crates/steward-types/src/util.ts"
expect_class full "rename from docs to scripts" "D 100644 000000 docs/run.md" "A 000000 100644 scripts/run.md"
expect_class full "workflow file" .github/workflows/ci.yml
expect_class full "docs plus a workflow file" docs/README.md .github/workflows/release.yml
expect_class full "composite action" .github/actions/setup-tools/action.yml
expect_class full "README beside workflows" .github/workflows/README.md
expect_class full "README beside migrations" migrations/README.md
expect_class full "README beside manifests" manifests/README.md
expect_class full "README beside a vendored patch" third_party/mcp-gw-patches/c2af10d9/README.md
expect_class full "README beside scripts" scripts/README.md
expect_class full "agent instructions in an unlisted root" build/AGENTS.md
expect_class full "README in a new root" packaging/README.md
expect_class full "README in a bundle below a new root" dist/bundle/README.md
expect_class docs "mode-only change is classified by path" "M 100644 100644 docs/README.md"
expect_class web "mode-only change under web" "M 100644 100755 web/scripts/build.sh"
expect_class full "git hook" .githooks/pre-push
expect_class full "root lockfile" bun.lock
expect_class full "web plus root lockfile" web/package.json bun.lock
expect_class full "Cargo lockfile" Cargo.lock
expect_class full "Cargo manifest" Cargo.toml
expect_class full "root package manifest" package.json
expect_class full "Bun version pin" .bun-version
expect_class full "Node version pin" .node-version
expect_class full "Rust toolchain pin" rust-toolchain.toml
expect_class full "web image build" build/web.Dockerfile
expect_class full "chart template" charts/steward/templates/all.yaml
expect_class full "chart metadata" charts/steward/Chart.yaml
expect_class full "changelog with chart metadata" CHANGELOG.md charts/steward/Chart.yaml
expect_class full "chart README is packaged" charts/steward/README.md
expect_class full "configuration bundle README is packaged" config/provider-profile-bundle/v1.2.2/README.md
expect_class full "product compatibility" config/product-compatibility/v1.json
expect_class full "script" scripts/ci-change-class.sh
expect_class full "xtask" xtask/src/main.rs
expect_class full "migration" migrations/0001_init.sql
expect_class full "adapter" adapters/openshell/src/lib.rs
expect_class full "binary" bins/steward-apiserver/src/main.rs
expect_class full "third-party notices are copied into images" THIRD_PARTY_NOTICES.md
expect_class full "license" LICENSE
expect_class full "agent permission settings" .claude/settings.json
expect_class full "script beside a skill" .claude/skills/review/run.sh
expect_class full "wire contract prose" docs/contracts/task/v2/README.md
expect_class full "wire contract fixture" docs/contracts/m1/v1/fixtures/positive/task-definition.json
expect_class full "example compiled into tests" docs/examples/github-file-read-v1.yaml
expect_class full "Markdown compiled with include_str" docs/task-submission-api.md
expect_class full "second Markdown compiled with include_str" docs/installation/installation-guide.md
expect_class full "Markdown copied into a bundle" docs/installation/platform-preflight.md
expect_class full "non-prose under docs" docs/installation/execution-bindings.example.json
expect_class full "root Markdown off the allowlist" NOTES.md
expect_class full "Markdown beside code" crates/steward-types/DESIGN.md
expect_class full "lookalike directory" website/index.html
expect_class full "case-changed directory" Web/src/app/page.tsx
expect_class full "symlink added under web" "A 000000 120000 web/src/link.ts"
expect_class full "symlink added under docs" "A 000000 120000 docs/link.md"
expect_class full "file replaced by a symlink" "M 100644 120000 docs/README.md"
expect_class full "symlink removed" "D 120000 000000 web/src/link.ts"
expect_class full "submodule under web" "A 000000 160000 web/vendor"
expect_class full "executable documentation" "A 000000 100755 docs/run.md"
expect_class full "documentation made executable" "M 100644 100755 README.md"
expect_class full "type change" "T 100644 100644 docs/README.md"
expect_class full "unmerged path" "U 100644 100644 web/src/app/page.tsx"
expect_class full "rename record" "R 100644 100644 web/src/app/page.tsx"
expect_class full "path with a control character" "$(printf 'docs/a\tb.md')"
expect_class full "path with a newline" "$(printf 'docs/a\nREADME.md')"
expect_class full "parent traversal" docs/../scripts/x.md
expect_class full "path with a space" "docs/release notes.md"
expect_class full "path with a workflow command marker" "docs/::notice::x.md"
expect_class full "path with a quote" "web/src/it's.ts"
expect_class full "path with a non-ASCII character" "docs/überblick.md"
expect_class web "route directory with brackets" "web/src/app/runs/[id]/logs/[stream]/page.tsx"
expect_class web "route group with parentheses" "web/src/app/(admin)/page.tsx"
expect_class full "absolute path" /docs/README.md

# Changed paths are untrusted. A file name that carries a runner workflow
# command must be `full`, and no line of output may start with `::` or repeat
# the name.
hostile_names=(
  "$(printf 'docs/a\n::add-mask::README.md')"
  "$(printf 'docs/a\n::stop-commands::token\n.md')"
  "$(printf 'web/src/a\r::error::spoofed.ts')"
  "$(printf 'crates/x\n::set-output name=class::docs')"
  'docs/::add-mask::x.md'
  'crates/a b::error::x.rs'
  "$(printf 'docs/\033[2K.md')"
)
for hostile in "${hostile_names[@]}"; do
  for status in M U; do
    output="$(raw_record "${status}" 100644 100644 "${hostile}" | bash "${script}" classify 2>&1)"
    [[ "$(tail -n 1 <<<"${output}")" == full ]] || fail "hostile path must be full"
    if grep -Eq '(^|[[:cntrl:]])::|add-mask|stop-commands|spoofed|set-output' <<<"${output}"; then
      fail "a hostile path reached the log: $(printf '%q' "${output}")"
    fi
    if [[ "$(wc -l <<<"${output}" | tr -d ' ')" != 2 ]]; then
      fail "a hostile path added log lines: $(printf '%q' "${output}")"
    fi
  done
done
output="$(bash "${script}" verdict "$(printf 'docs\n::add-mask::x')" "$(printf 'success\n::error::y')" \
  "$(printf 'quality=success\n::stop-commands::z')" 2>&1)" && fail "a hostile verdict input must fail"
if grep -Eq '(^|[[:cntrl:]])::|add-mask|stop-commands' <<<"${output}"; then
  fail "a hostile verdict input reached the log"
fi

# An empty diff, malformed input, and an oversized change set.
if [[ "$(printf '' | bash "${script}" classify 2>/dev/null)" != full ]]; then
  fail "empty diff must be full"
fi
if [[ "$(printf 'docs/README.md\0' | bash "${script}" classify 2>/dev/null)" != full ]]; then
  fail "a record without metadata must be full"
fi
if [[ "$(printf ':100644 100644 %s %s M\0' "${zero}" "${blob}" | bash "${script}" classify 2>/dev/null)" != full ]]; then
  fail "a record without a path must be full"
fi
if [[ "$(printf ':100644 100644 %s %s M\0docs/README.md' "${zero}" "${blob}" | bash "${script}" classify 2>/dev/null)" != full ]]; then
  fail "an unterminated record must be full"
fi

many_entries() {
  local count="$1" index
  for ((index = 0; index < count; index++)); do
    raw_record M 100644 100644 "docs/page-${index}.md"
  done
}
if [[ "$(many_entries "${MAX_CHANGED_FILES}" | bash "${script}" classify 2>/dev/null)" != docs ]]; then
  fail "a change set at the size limit must still be classified"
fi
if [[ "$(many_entries "$((MAX_CHANGED_FILES + 1))" | bash "${script}" classify 2>/dev/null)" != full ]]; then
  fail "a change set over the size limit must be full"
fi

# ---------------------------------------------------------------------------
# detect: real git history for each event type.
# ---------------------------------------------------------------------------

repository="${scratch}/repository"
mkdir -p "${repository}"

fixture_git() {
  env -u GIT_DIR -u GIT_WORK_TREE -u GIT_INDEX_FILE -u GIT_COMMON_DIR -u GIT_PREFIX \
    GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null GIT_CONFIG_NOSYSTEM=1 \
    GIT_AUTHOR_NAME=alice GIT_AUTHOR_EMAIL=alice@example.com \
    GIT_COMMITTER_NAME=alice GIT_COMMITTER_EMAIL=alice@example.com \
    git -C "${repository}" -c commit.gpgsign=false -c core.symlinks=true \
    -c protocol.file.allow=never "$@"
}

fixture_commit() {
  fixture_git add -A
  fixture_git commit --quiet --allow-empty -m "$1"
  fixture_git rev-parse HEAD
}

fixture_git init --quiet --initial-branch=main
mkdir -p "${repository}/web/src" "${repository}/crates/steward-types/src" \
  "${repository}/docs" "${repository}/tests/browser"
echo 'export const a = 1;' >"${repository}/web/src/a.ts"
echo 'pub fn a() {}' >"${repository}/crates/steward-types/src/a.rs"
echo '# Guide' >"${repository}/docs/guide.md"
echo '# Rules' >"${repository}/AGENTS.md"
echo '# Changes' >"${repository}/CHANGELOG.md"
base="$(fixture_commit base)"

fixture_git update-ref refs/remotes/origin/main "${base}"

# Each scenario branches from the base commit and returns its head.
scenario() {
  fixture_git checkout --quiet --detach "${base}"
}

expect_detect() {
  local expected="$1" name="$2" event="$3" base_ref="$4" base_sha="$5" head_sha="$6"
  local observed status=0
  observed="$(cd "${repository}" && env -u GIT_DIR -u GIT_WORK_TREE \
    CI_EVENT_NAME="${event}" CI_BASE_REF="${base_ref}" \
    CI_BASE_SHA="${base_sha}" CI_HEAD_SHA="${head_sha}" \
    bash "${script}" detect 2>/dev/null)" || status=$?
  if [[ "${status}" != 0 ]]; then
    fail "${name}: detect exited with ${status}"
  elif [[ "${observed}" != "${expected}" ]]; then
    fail "${name}: expected ${expected}, got ${observed}"
  fi
}

scenario
echo 'export const a = 2;' >"${repository}/web/src/a.ts"
web_head="$(fixture_commit 'web change')"

scenario
echo '# Guide, revised' >"${repository}/docs/guide.md"
echo '# Rules, revised' >"${repository}/AGENTS.md"
echo '# Changes, revised' >"${repository}/CHANGELOG.md"
docs_head="$(fixture_commit 'docs change')"

scenario
echo 'pub fn b() {}' >"${repository}/crates/steward-types/src/a.rs"
rust_head="$(fixture_commit 'rust change')"

scenario
fixture_git mv web/src/a.ts crates/steward-types/src/a.ts
rename_head="$(fixture_commit 'rename from web to crates')"

scenario
fixture_git rm --quiet crates/steward-types/src/a.rs
delete_head="$(fixture_commit 'delete a Rust file')"

scenario
ln -s ../../crates/steward-types/src/a.rs "${repository}/web/src/link.ts"
symlink_head="$(fixture_commit 'symlink under web')"

scenario
hostile_file="$(printf 'docs/a\n::add-mask::README.md')"
echo '# hostile' >"${repository}/${hostile_file}"
hostile_head="$(fixture_commit 'file name with a workflow command')"

scenario
mkdir -p "${repository}/.github/workflows"
echo 'name: ci' >"${repository}/.github/workflows/ci.yml"
workflow_head="$(fixture_commit 'workflow change')"

scenario
echo '{}' >"${repository}/bun.lock"
echo '{}' >"${repository}/web/package.json"
lockfile_head="$(fixture_commit 'lockfile change')"

scenario
echo 'export const a = 3;' >"${repository}/web/src/a.ts"
fixture_commit 'web change in a group' >/dev/null
echo 'pub fn c() {}' >"${repository}/crates/steward-types/src/a.rs"
mixed_group_head="$(fixture_commit 'rust change in the same group')"

scenario
echo 'export const a = 4;' >"${repository}/web/src/a.ts"
fixture_commit 'first web change in a group' >/dev/null
echo '# Guide for the group' >"${repository}/docs/guide.md"
web_group_head="$(fixture_commit 'docs change in the same group')"

scenario
for ((index = 0; index <= MAX_CHANGED_FILES; index++)); do
  echo "# Page ${index}" >"${repository}/docs/page-${index}.md"
done
large_head="$(fixture_commit 'large docs change')"

# A pull request is tested as a merge commit whose first parent can be newer
# than the base recorded in the event. Here main gained a Rust change that the
# pull request reverts while also editing web: against the event base only the
# web file differs, against the real merge parent the Rust file differs too.
scenario
echo 'pub fn moved() {}' >"${repository}/crates/steward-types/src/a.rs"
moved_main="$(fixture_commit 'main moved')"
echo 'pub fn a() {}' >"${repository}/crates/steward-types/src/a.rs"
echo 'export const a = 5;' >"${repository}/web/src/a.ts"
fixture_git add -A
merge_tree="$(fixture_git write-tree)"
stale_base_merge="$(fixture_git commit-tree "${merge_tree}" -p "${moved_main}" -p "${web_head}" -m 'merge')"

# A documentation entry queued behind a Rust entry that has not merged yet.
fixture_git checkout --quiet --force --detach "${rust_head}"
echo '# Guide, queued' >"${repository}/docs/guide.md"
queued_docs_head="$(fixture_commit 'docs change queued behind a Rust change')"
fixture_git checkout --quiet --force --detach "${web_head}"
echo '# Guide, queued behind web' >"${repository}/docs/guide.md"
queued_behind_web_head="$(fixture_commit 'docs change queued behind a web change')"

unrelated="$(fixture_git commit-tree "$(fixture_git rev-parse "${base}^{tree}")" -m 'unrelated root')"
missing=2222222222222222222222222222222222222222

expect_detect web "pull request, web only" pull_request main "${base}" "${web_head}"
expect_detect docs "pull request, docs only" pull_request main "${base}" "${docs_head}"
expect_detect full "pull request, Rust" pull_request main "${base}" "${rust_head}"
expect_detect full "pull request, rename from web to crates" pull_request main "${base}" "${rename_head}"
expect_detect full "pull request, deleted Rust file" pull_request main "${base}" "${delete_head}"
expect_detect full "pull request, symlink under web" pull_request main "${base}" "${symlink_head}"
expect_detect full "pull request, hostile file name" pull_request main "${base}" "${hostile_head}"
output="$(cd "${repository}" && env -u GIT_DIR -u GIT_WORK_TREE \
  CI_EVENT_NAME=pull_request CI_BASE_REF=main CI_BASE_SHA="${base}" CI_HEAD_SHA="${hostile_head}" \
  bash "${script}" detect 2>&1)"
if grep -Eq '(^|[[:cntrl:]])::|add-mask' <<<"${output}"; then
  fail "a hostile file name reached the detect log"
fi
output="$(cd "${repository}" && env -u GIT_DIR -u GIT_WORK_TREE \
  CI_EVENT_NAME="$(printf 'push\n::add-mask::x')" CI_BASE_REF="$(printf 'main\n::error::x')" \
  CI_BASE_SHA="${base}" CI_HEAD_SHA="${web_head}" bash "${script}" detect 2>&1)"
if grep -Eq '(^|[[:cntrl:]])::|add-mask' <<<"${output}"; then
  fail "a hostile event name reached the detect log"
fi
expect_detect full "pull request, workflow file" pull_request main "${base}" "${workflow_head}"
expect_detect full "pull request, lockfile" pull_request main "${base}" "${lockfile_head}"
expect_detect full "pull request, oversized" pull_request main "${base}" "${large_head}"
expect_detect full "pull request, empty diff" pull_request main "${base}" "${base}"
expect_detect full "pull request, stale event base" pull_request main "${base}" "${stale_base_merge}"
expect_detect full "pull request to a release branch" pull_request release/0.3 "${base}" "${web_head}"
expect_detect full "pull request, missing base" pull_request main "" "${web_head}"
expect_detect full "pull request, missing head" pull_request main "${base}" ""
expect_detect full "pull request, abbreviated base" pull_request main "${base:0:12}" "${web_head}"
expect_detect full "pull request, unknown base" pull_request main "${missing}" "${web_head}"
expect_detect full "pull request, base is not an ancestor" pull_request main "${unrelated}" "${web_head}"
expect_detect full "pull request, base is a ref name" pull_request main main "${web_head}"

expect_detect web "merge group, web only" merge_group refs/heads/main "${base}" "${web_head}"
expect_detect web "merge group, web and docs commits" merge_group refs/heads/main "${base}" "${web_group_head}"
expect_detect docs "merge group, docs only" merge_group refs/heads/main "${base}" "${docs_head}"
expect_detect full "merge group, web and Rust commits" merge_group refs/heads/main "${base}" "${mixed_group_head}"
expect_detect full "merge group, docs queued behind Rust" merge_group refs/heads/main "${rust_head}" "${queued_docs_head}"
expect_detect web "merge group, docs queued behind web" merge_group refs/heads/main "${web_head}" "${queued_behind_web_head}"
observed="$(cd "${repository}" && env -u GIT_DIR -u GIT_WORK_TREE CI_TARGET_REF=refs/remotes/origin/absent \
  CI_EVENT_NAME=merge_group CI_BASE_REF=refs/heads/main CI_BASE_SHA="${base}" CI_HEAD_SHA="${docs_head}" \
  bash "${script}" detect 2>/dev/null)" || fail "detect without a main ref exited non-zero"
[[ "${observed}" == full ]] || fail "merge group without a main ref: expected full, got ${observed}"
expect_detect full "merge group, missing base" merge_group refs/heads/main "" "${web_head}"
expect_detect full "merge group for another branch" merge_group refs/heads/release/0.3 "${base}" "${web_head}"
expect_detect full "merge group, short base ref" merge_group main "${base}" "${web_head}"

for event in push schedule workflow_dispatch release pull_request_target ""; do
  expect_detect full "event '${event}', web only" "${event}" main "${base}" "${web_head}"
  expect_detect full "event '${event}', docs only" "${event}" refs/heads/main "${base}" "${docs_head}"
done

# Outside a repository the classifier still answers, and answers full.
outside="${scratch}/outside"
mkdir -p "${outside}"
observed="$(cd "${outside}" && env -u GIT_DIR -u GIT_WORK_TREE GIT_CEILING_DIRECTORIES="${scratch}" \
  CI_EVENT_NAME=pull_request CI_BASE_REF=main CI_BASE_SHA="${base}" CI_HEAD_SHA="${web_head}" \
  bash "${script}" detect 2>/dev/null)" || fail "detect outside a repository exited non-zero"
[[ "${observed}" == full ]] || fail "detect outside a repository: expected full, got ${observed}"

# ---------------------------------------------------------------------------
# verdict: what the aggregate accepts for each class.
# ---------------------------------------------------------------------------

expect_verdict() {
  local expected="$1" name="$2"
  shift 2
  local status=0
  bash "${script}" verdict "$@" >/dev/null 2>&1 || status=$?
  if [[ "${expected}" == pass && "${status}" != 0 ]]; then
    fail "verdict ${name}: expected pass, exited ${status}"
  elif [[ "${expected}" == fail && "${status}" == 0 ]]; then
    fail "verdict ${name}: expected failure, passed"
  fi
}

full_lanes=(
  quality=success quality-reduced=skipped conformance-pinned=success
  openshell-adapter=success governed-connections=success postgres-tls=success
  browser-e2e=success codex-reference-runtime=success release-candidate=success
)
web_lanes=(
  quality=skipped quality-reduced=success conformance-pinned=skipped
  openshell-adapter=skipped governed-connections=skipped postgres-tls=skipped
  browser-e2e=success codex-reference-runtime=skipped release-candidate=success
)
docs_lanes=(
  quality=skipped quality-reduced=success conformance-pinned=skipped
  openshell-adapter=skipped governed-connections=skipped postgres-tls=skipped
  browser-e2e=skipped codex-reference-runtime=skipped release-candidate=skipped
)

# Replaces one lane's result in a lane list.
with_result() {
  local replacement="$1"
  shift
  local argument
  for argument in "$@"; do
    if [[ "${argument%%=*}" == "${replacement%%=*}" ]]; then
      echo "${replacement}"
    else
      echo "${argument}"
    fi
  done
}

expect_verdict pass "full, all lanes green" full success "${full_lanes[@]}"
expect_verdict pass "web, web lanes green" web success "${web_lanes[@]}"
expect_verdict pass "docs, docs lanes green" docs success "${docs_lanes[@]}"

functional_lanes=(
  quality conformance-pinned openshell-adapter governed-connections
  postgres-tls browser-e2e codex-reference-runtime release-candidate
)
for lane in "${functional_lanes[@]}"; do
  for result in skipped failure cancelled ""; do
    # shellcheck disable=SC2046 # one argument per line is intended
    expect_verdict fail "full, ${lane}=${result}" full success \
      $(with_result "${lane}=${result}" "${full_lanes[@]}")
  done
done

for lane in quality-reduced browser-e2e release-candidate; do
  for result in skipped failure cancelled; do
    # shellcheck disable=SC2046
    expect_verdict fail "web, ${lane}=${result}" web success \
      $(with_result "${lane}=${result}" "${web_lanes[@]}")
  done
done
for result in skipped failure cancelled; do
  # shellcheck disable=SC2046
  expect_verdict fail "docs, quality-reduced=${result}" docs success \
    $(with_result "quality-reduced=${result}" "${docs_lanes[@]}")
done

# A lane the class skips must really have been skipped: a failure or a
# cancellation there is never waved through.
for lane in quality conformance-pinned openshell-adapter governed-connections postgres-tls codex-reference-runtime; do
  for result in failure cancelled success; do
    # shellcheck disable=SC2046
    expect_verdict fail "web, ${lane}=${result}" web success \
      $(with_result "${lane}=${result}" "${web_lanes[@]}")
    # shellcheck disable=SC2046
    expect_verdict fail "docs, ${lane}=${result}" docs success \
      $(with_result "${lane}=${result}" "${docs_lanes[@]}")
  done
done
# shellcheck disable=SC2046
expect_verdict fail "docs, browser-e2e=failure" docs success \
  $(with_result "browser-e2e=failure" "${docs_lanes[@]}")

# The results of one class never satisfy another.
expect_verdict fail "full with web results" full success "${web_lanes[@]}"
expect_verdict fail "full with docs results" full success "${docs_lanes[@]}"
expect_verdict fail "web with docs results" web success "${docs_lanes[@]}"
expect_verdict fail "docs with web results" docs success "${web_lanes[@]}"
expect_verdict fail "web with full results" web success "${full_lanes[@]}"

# The classifier itself.
for result in failure cancelled skipped ""; do
  expect_verdict fail "classifier ${result}, full" full "${result}" "${full_lanes[@]}"
  expect_verdict fail "classifier ${result}, docs" docs "${result}" "${docs_lanes[@]}"
done
for class in "" unknown FULL "docs web" "web "; do
  expect_verdict fail "unknown class '${class}'" "${class}" success "${docs_lanes[@]}"
  expect_verdict fail "unknown class '${class}' with full results" "${class}" success "${full_lanes[@]}"
done

# Missing, duplicated, and unknown lanes.
expect_verdict fail "no lane results" docs success
expect_verdict fail "missing lane" full success "${full_lanes[@]:1}"
expect_verdict fail "duplicated lane" docs success "${docs_lanes[@]}" quality-reduced=success
expect_verdict fail "unknown lane" docs success "${docs_lanes[@]}" extra-lane=success
expect_verdict fail "malformed lane" docs success "${docs_lanes[@]}" quality-reduced

# ---------------------------------------------------------------------------
# Consumer guard: a file is `docs` only while nothing compiles or bundles it.
# These checks read the real repository and fail when a new consumer appears
# for a path the classifier still calls docs. The fix is to add that path to
# the consumed list in ci-change-class.sh, never to relax the check.
# ---------------------------------------------------------------------------

repo_git() {
  env -u GIT_DIR -u GIT_WORK_TREE -u GIT_INDEX_FILE git -C "${root}" "$@"
}

if repo_git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  tracked="${scratch}/tracked"
  repo_git ls-files >"${tracked}"

  is_tracked() {
    grep -Fxq -- "$1" "${tracked}"
  }

  # Prints a path with `.` and `..` segments resolved, or nothing when it
  # escapes the repository.
  normalize() {
    local path="$1" result="" segment rest
    rest="${path}"
    while [[ -n "${rest}" ]]; do
      segment="${rest%%/*}"
      if [[ "${rest}" == */* ]]; then rest="${rest#*/}"; else rest=""; fi
      case "${segment}" in
        ''|.) ;;
        ..)
          [[ -n "${result}" ]] || return 0
          if [[ "${result}" == */* ]]; then result="${result%/*}"; else result=""; fi
          ;;
        *) result="${result:+${result}/}${segment}" ;;
      esac
    done
    echo "${result}"
  }

  # The directory of the nearest Cargo.toml at or above a source file.
  crate_root() {
    local directory="${1%/*}"
    while [[ "${directory}" == */* ]]; do
      if is_tracked "${directory}/Cargo.toml"; then
        echo "${directory}"
        return
      fi
      directory="${directory%/*}"
    done
    if is_tracked "${directory}/Cargo.toml"; then echo "${directory}"; fi
  }

  # Fails when `candidate` names a docs-class file, or a directory holding one.
  reject_docs_consumer() {
    local consumer="$1" candidate="$2" member
    [[ -n "${candidate}" ]] || return 0
    while IFS= read -r member; do
      if [[ "$(entry_class 100644 100644 "${member}")" == docs ]]; then
        fail "${consumer} consumes ${member}, which is still classified docs"
      fi
    done < <(grep -E -- "^$(printf '%s' "${candidate}" | sed 's/[][\.*^$+?(){}|/]/\\&/g')(/|$)" "${tracked}" || true)
  }

  # 1. Rust sources: every string literal that could name a document,
  #    resolved against the source directory, its crate root, and the
  #    repository root. This covers include_str!, include_bytes!, and files
  #    read at test time. Only the test modules of xtask are exempt, because
  #    the reduced lanes run `cargo test -p xtask`; the xtask commands
  #    themselves run only in the full gate.
  xtask_test_start() {
    local line
    line="$(grep -n '^#\[cfg(test)\]' "${root}/$1" | tail -n 1 | cut -d: -f1)"
    echo "${line:-0}"
  }
  while IFS= read -r match; do
    source_file="${match%%:*}"
    match="${match#*:}"
    line_number="${match%%:*}"
    literal="${match#*:}"
    in_xtask=0
    if [[ "${source_file}" == xtask/* ]]; then
      in_xtask=1
      test_start="$(xtask_test_start "${source_file}")"
      if ((test_start > 0 && line_number >= test_start)); then
        continue
      fi
    fi
    literal="${literal#\"}"
    literal="${literal%\"}"
    case "${literal}" in
      *://*|*' '*|'') continue ;;
    esac
    # Only a leading run of ../ is a plausible build-time path.
    trimmed="${literal#/}"
    while [[ "${trimmed}" == ../* ]]; do trimmed="${trimmed#../}"; done
    case "${trimmed}" in
      *../*|*/..) continue ;;
    esac
    reject_docs_consumer "${source_file}" "$(normalize "${source_file%/*}/${literal}")"
    crate="$(crate_root "${source_file}")"
    if [[ -n "${crate}" ]]; then
      reject_docs_consumer "${source_file}" "$(normalize "${crate}/${literal}")"
    fi
    if [[ "${literal}" == */* || "${in_xtask}" == 1 ]]; then
      reject_docs_consumer "${source_file}" "$(normalize "${literal}")"
    fi
  done < <(repo_git grep -noE '"[^"]*(docs/|\.md|\.txt|\.rst|README|AGENTS|CLAUDE|CHANGELOG|\.png|\.jpe?g|\.gif|\.svg|\.webp)[^"]*"' \
    -- '*.rs' || true)
  for xtask_source in xtask/src/main.rs xtask/src/lib.rs; do
    (($(xtask_test_start "${xtask_source}") > 0)) ||
      fail "${xtask_source} has no test module marker; the xtask exemption cannot be scoped"
  done

  # 2. Packaged directories, derived from the build definitions: everything
  #    the final stage of an image copies, from the build context or from an
  #    earlier stage that holds the repository, and every chart directory
  #    passed to `helm package`. No docs-class file may live in one, so a new
  #    packaged directory fails here until its files are classified. Earlier
  #    image stages only feed the compiler, which check 1 covers.
  packaged="${scratch}/packaged"
  : >"${packaged}"
  packaged_candidate() {
    local consumer="$1" candidate="$2"
    [[ -n "${candidate}" ]] || return 0
    if grep -Eq -- "^$(printf '%s' "${candidate}" | sed 's/[][\.*^$+?(){}|/]/\\&/g')(/|$)" "${tracked}"; then
      echo "${candidate}" >>"${packaged}"
      reject_docs_consumer "${consumer}" "${candidate}"
    fi
  }
  while IFS= read -r dockerfile; do
    while IFS= read -r instruction; do
      read -r -a tokens <<<"${instruction}"
      token_count="${#tokens[@]}"
      from_stage=0
      for ((index = 1; index < token_count - 1; index++)); do
        token="${tokens[index]}"
        case "${token}" in
          --from=*) from_stage=1; continue ;;
          --*) continue ;;
        esac
        if ((from_stage == 0)); then
          case "${token}" in
            .|./|'*'|./'*')
              fail "${dockerfile} copies the whole build context into its final image"
              continue
              ;;
          esac
          packaged_candidate "${dockerfile}" "$(normalize "${token}")"
        else
          # A path inside an earlier stage: try each suffix as a repository
          # path, so /workspace/web/public is recognised as web/public.
          suffix="${token#/}"
          while [[ -n "${suffix}" ]]; do
            packaged_candidate "${dockerfile}" "$(normalize "${suffix}")"
            if [[ "${suffix}" == */* ]]; then suffix="${suffix#*/}"; else suffix=""; fi
          done
        fi
      done
    done < <(awk '
      /^FROM[[:space:]]/ { count = 0; next }
      toupper($1) == "COPY" || toupper($1) == "ADD" { lines[count++] = $0 }
      END { for (i = 0; i < count; i++) print lines[i] }
    ' "${root}/${dockerfile}")
    if grep -Eq '^(COPY|ADD)[[:space:]].*\\$' "${root}/${dockerfile}"; then
      fail "${dockerfile} continues a COPY across lines, which this guard does not parse"
    fi
  done < <(grep -E '(^|/)[^/]*Dockerfile[^/]*$' "${tracked}" || true)

  while IFS= read -r match; do
    consumer="${match%%:*}"
    arguments="${match#*helm package}"
    read -r -a tokens <<<"${arguments}"
    token="${tokens[0]:-}"
    token="${token//\"/}"
    token="${token#\$\{root\}/}"
    token="${token#\$root/}"
    token="${token#\$\{ROOT\}/}"
    candidate="$(normalize "${token}")"
    if [[ -z "${candidate}" ]] || ! grep -q -- "^${candidate}/" "${tracked}"; then
      fail "${consumer} packages a chart directory this guard cannot resolve: ${token}"
      continue
    fi
    packaged_candidate "${consumer}" "${candidate}"
  done < <(repo_git grep -E 'helm package' -- '*.sh' '.github' ':!scripts/test-ci-change-class.sh' || true)

  # The derivation must see the directories known to be packaged today.
  for expected in charts/steward web/public; do
    grep -Fxq -- "${expected}" "${packaged}" ||
      fail "the packaged-directory derivation no longer finds ${expected}"
  done

  # 3. Scripts, workflows, and chart sources that name a docs-class path.
  #    Root-level names such as README.md are too common to match as text;
  #    their readers are listed in AGENTS.md and run in the reduced lanes.
  patterns="${scratch}/patterns"
  : >"${patterns}"
  while IFS= read -r path; do
    if [[ "${path}" == */* && "$(entry_class 100644 100644 "${path}")" == docs ]]; then
      echo "${path}" >>"${patterns}"
    fi
  done <"${tracked}"
  if [[ -s "${patterns}" ]]; then
    while IFS= read -r match; do
      fail "${match%%:*} names a path that is still classified docs: ${match#*:}"
    done < <(repo_git grep -nFf "${patterns}" -- \
      '*.sh' '*.py' '*Dockerfile*' '.github' 'charts' 'build' '.githooks' \
      ':!*.md' \
      ':!scripts/test-ci-change-class.sh' \
      ':!scripts/validate-release-version.sh' \
      ':!scripts/test-validate-release-version.sh' || true)
  fi

  # 4. The browser presentation must not reach documents outside web/:
  #    no relative path into docs/, and no relative Markdown or text import.
  while IFS= read -r match; do
    fail "web reaches a document outside web/: ${match}"
  done < <(repo_git grep -nE "['\"\`](\.\./)+(docs/[^'\"\`]*|[^'\"\`]*\.(md|mdx|txt|rst))['\"\`]" \
    -- web tests/browser || true)

  # The guard itself must see the known consumers, or it is not looking.
  for consumed in \
    docs/task-submission-api.md \
    docs/installation/installation-guide.md \
    docs/installation/federated-task-identity-upgrade.md \
    docs/installation/platform-preflight.md \
    charts/steward/README.md
  do
    is_tracked "${consumed}" || fail "consumed document ${consumed} is no longer tracked; update the consumed list"
    [[ "$(entry_class 100644 100644 "${consumed}")" == full ]] ||
      fail "${consumed} is compiled or bundled and must be classified full"
  done
  repo_git grep -qF 'include_str!("../../../docs/task-submission-api.md")' -- crates ||
    fail "the include_str! consumer the guard is calibrated against has moved; re-check the consumed list"

  # Every tracked agent-instruction file and README at the root or below a
  # listed source root is docs, as is the changelog.
  while IFS= read -r path; do
    case "${path}" in
      docs/contracts/*) continue ;;
      */*)
        case "${path}" in
          adapters/*|bins/*|conformance/*|crates/*|docs/*|e2e/*|xtask/*) ;;
          *) continue ;;
        esac
        ;;
    esac
    case "${path##*/}" in
      AGENTS.md|CLAUDE.md|README.md)
        [[ "$(entry_class 100644 100644 "${path}")" == docs ]] ||
          fail "${path} must be classified docs"
        ;;
    esac
  done <"${tracked}"
  [[ "$(entry_class 100644 100644 CHANGELOG.md)" == docs ]] || fail "CHANGELOG.md must be classified docs"
  while IFS= read -r path; do
    [[ "$(entry_class 100644 100644 "${path}")" == docs ]] || fail "${path} must be classified docs"
  done < <(grep -E '^\.claude/.*\.md$' "${tracked}" || true)
fi

if ((failures != 0)); then
  echo "ci change class: ${failures} failure(s)" >&2
  exit 1
fi
echo 'ci change class contract passed'
