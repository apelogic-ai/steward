#!/usr/bin/env bash
# Path-aware CI: classifies a change set and evaluates the aggregate verdict.
#
#   ci-change-class.sh classify   reads `git diff --raw -z --no-renames` on
#                                 stdin and prints docs, web, or full
#   ci-change-class.sh detect     computes the diff for the current CI event
#                                 and prints docs, web, or full
#   ci-change-class.sh verdict <class> <classifier-result> <lane>=<result>...
#                                 exits 0 only when every lane ended the way
#                                 the class requires
#
# Every rule is an allowlist. A path is `docs` or `web` only when a rule below
# names it; everything else, and every failure or ambiguity, is `full`.
set -euo pipefail

# A larger change set is not worth reasoning about path by path.
MAX_CHANGED_FILES=300

# What each class runs. `run` lanes must end `success`; `skip` lanes must end
# `skipped`. Any other combination fails the aggregate, so this table and the
# job conditions in .github/workflows/ci.yml cannot drift apart silently.
#
# lane                     full  web   docs
LANES='
quality                    run   skip  skip
quality-reduced            skip  run   run
conformance-pinned         run   skip  skip
openshell-adapter          run   skip  skip
governed-connections       run   skip  skip
postgres-tls               run   skip  skip
browser-e2e                run   run   skip
codex-reference-runtime    run   skip  skip
release-candidate          run   run   skip
'

usage() {
  cat >&2 <<'USAGE'
usage: ci-change-class.sh classify < raw-diff
       ci-change-class.sh detect
       ci-change-class.sh verdict <class> <classifier-result> <lane>=<result>...
USAGE
  exit 2
}

# Prints the class of one repository-relative path, ignoring its file mode.
path_class() {
  local path="$1"
  local name="${path##*/}"

  # The browser presentation and its journeys. Everything below these
  # directories is `web`, including Markdown: files under web/ can be bundled
  # into the web image, so none of them is `docs`.
  case "${path}" in
    web/*|tests/browser/*)
      echo web
      return
      ;;
  esac

  # Documentation-shaped files that are compiled, bundled, or read by a gate
  # the reduced lanes do not run. They are classified by their consumer.
  case "${path}" in
    # CI and hook configuration.
    .github/*|.githooks/*) echo full; return ;;
    # Packaged into the Helm chart or a released configuration bundle.
    charts/*|config/*) echo full; return ;;
    # Normative wire contracts and examples read by crate and e2e tests.
    docs/contracts/*|docs/examples/*) echo full; return ;;
    # Compiled into apiserver tests with include_str!.
    docs/task-submission-api.md) echo full; return ;;
    docs/installation/installation-guide.md) echo full; return ;;
    docs/installation/federated-task-identity-upgrade.md) echo full; return ;;
    # Copied into the platform-preflight bundle.
    docs/installation/platform-preflight.md) echo full; return ;;
  esac

  # Agent instructions and READMEs, wherever they live.
  case "${name}" in
    AGENTS.md|CLAUDE.md|README|README.md|README.txt|README.rst)
      echo docs
      return
      ;;
  esac

  case "${path}" in
    CHANGELOG.md) echo docs ;;
    # Agent and skill definitions. Settings and scripts beside them are not
    # Markdown and stay `full`.
    .claude/*.md|.codex/*.md) echo docs ;;
    # Prose and static images.
    docs/*.md|docs/*.png|docs/*.jpg|docs/*.jpeg|docs/*.gif|docs/*.svg|docs/*.webp)
      echo docs
      ;;
    *) echo full ;;
  esac
}

# Prints the class of one changed entry: its path plus the file modes on both
# sides of the diff. 000000 is the absent side of an addition or a deletion.
entry_class() {
  local old_mode="$1" new_mode="$2" path="$3"
  local mode class

  for mode in "${old_mode}" "${new_mode}"; do
    case "${mode}" in
      000000|100644|100755) ;;
      # A symlink, a submodule, or a mode this script does not know.
      *) echo full; return ;;
    esac
  done

  case "${path}" in
    ''|/*|../*|*/../*|*/..|*[[:cntrl:]]*)
      echo full
      return
      ;;
  esac

  class="$(path_class "${path}")"
  if [[ "${class}" == docs && ( "${old_mode}" == 100755 || "${new_mode}" == 100755 ) ]]; then
    # Executable documentation is a script.
    class=full
  fi
  echo "${class}"
}

# Reads `git diff --raw -z --no-renames` records and prints the class of the
# whole change set. With --no-renames a rename is a deletion plus an addition,
# so both of its paths are classified.
classify() {
  local class=docs count=0
  local meta="" path="" old_mode new_mode status entry

  while IFS= read -r -d '' meta; do
    path=""
    if ! IFS= read -r -d '' path; then
      echo "change class: full (truncated diff record)" >&2
      echo full
      return
    fi
    case "${meta}" in
      :*) ;;
      *)
        echo "change class: full (unrecognized diff record)" >&2
        echo full
        return
        ;;
    esac
    old_mode="" new_mode="" status=""
    read -r old_mode new_mode _ _ status <<<"${meta#:}" || true
    case "${status}" in
      A|D|M) ;;
      *)
        echo "change class: full (status '${status}' for ${path})" >&2
        echo full
        return
        ;;
    esac

    count=$((count + 1))
    if ((count > MAX_CHANGED_FILES)); then
      echo "change class: full (more than ${MAX_CHANGED_FILES} changed files)" >&2
      echo full
      return
    fi

    entry="$(entry_class "${old_mode}" "${new_mode}" "${path}")"
    case "${entry}" in
      docs) ;;
      web) class=web ;;
      *)
        echo "change class: full (${path})" >&2
        echo full
        return
        ;;
    esac
    meta=""
  done

  if [[ -n "${meta}" ]]; then
    echo "change class: full (unterminated diff record)" >&2
    echo full
    return
  fi
  if ((count == 0)); then
    echo "change class: full (empty diff)" >&2
    echo full
    return
  fi
  echo "change class: ${class} (${count} changed files)" >&2
  echo "${class}"
}

full_because() {
  echo "change class: full ($1)" >&2
  echo full
}

is_commit_id() {
  [[ "$1" =~ ^[0-9a-f]{40}$ ]]
}

raw_diff() {
  git diff --raw -z --no-renames --no-abbrev --no-ext-diff --no-textconv \
    --ignore-submodules=none "$1" "$2" --
}

detect_unchecked() {
  local event="${CI_EVENT_NAME:-}"
  local base_ref="${CI_BASE_REF:-}"
  local base="${CI_BASE_SHA:-}"
  local head="${CI_HEAD_SHA:-}"
  local target_ref="${CI_TARGET_REF:-refs/remotes/origin/main}"
  local parents first_parent diff queue_base

  # Pushes, schedules, manual runs, and anything not aimed at main always run
  # the full gate. Only the two pre-merge events can be reduced.
  case "${event}" in
    pull_request)
      if [[ "${base_ref}" != main ]]; then
        full_because "pull request targets '${base_ref}', not main"
        return
      fi
      ;;
    merge_group)
      if [[ "${base_ref}" != refs/heads/main ]]; then
        full_because "merge group targets '${base_ref}', not main"
        return
      fi
      ;;
    *)
      full_because "event '${event}' always runs the full gate"
      return
      ;;
  esac

  if ! is_commit_id "${base}" || ! is_commit_id "${head}"; then
    full_because "missing or malformed base or head commit"
    return
  fi
  if ! git cat-file -e "${base}^{commit}" 2>/dev/null ||
    ! git cat-file -e "${head}^{commit}" 2>/dev/null; then
    full_because "base or head commit is not available"
    return
  fi
  if ! git merge-base --is-ancestor "${base}" "${head}" 2>/dev/null; then
    full_because "base is not an ancestor of head"
    return
  fi

  diff="$(mktemp)"
  # shellcheck disable=SC2064 # expand the temporary path now
  trap "rm -f '${diff}'" EXIT

  if ! raw_diff "${base}" "${head}" >"${diff}"; then
    full_because "git could not compute the diff"
    return
  fi

  if [[ "${event}" == pull_request ]]; then
    # A pull request is tested as a merge commit. Its first parent is the
    # tip it was merged onto, which can be newer than the base in the event
    # payload. Classify the union so neither view can hide a path.
    parents="$(git rev-list --parents -n 1 "${head}")" || {
      full_because "git could not read the head commit"
      return
    }
    read -r _ first_parent _ <<<"${parents}" || true
    if [[ -n "${first_parent:-}" && "${first_parent}" != "${base}" ]]; then
      if ! raw_diff "${first_parent}" "${head}" >>"${diff}"; then
        full_because "git could not compute the merge diff"
        return
      fi
    fi
  fi

  if [[ "${event}" == merge_group ]]; then
    # The group's base is its parent in the queue, which can be another
    # group that has not merged yet. Classify everything between main and
    # this group, so an entry queued behind a non-web change is `full`
    # whatever the queue's merge strategy.
    if ! queue_base="$(git merge-base "${target_ref}" "${head}" 2>/dev/null)" ||
      ! is_commit_id "${queue_base}"; then
      full_because "cannot find where the merge group left main"
      return
    fi
    if [[ "${queue_base}" != "${base}" ]]; then
      if ! raw_diff "${queue_base}" "${head}" >>"${diff}"; then
        full_because "git could not compute the queue diff"
        return
      fi
    fi
  fi

  classify <"${diff}"
}

# Never fails and never prints anything but docs, web, or full.
detect() {
  local class
  class="$(detect_unchecked)" || class=full
  case "${class}" in
    docs|web|full) ;;
    *) class=full ;;
  esac
  echo "${class}"
}

verdict() {
  [[ $# -ge 2 ]] || usage
  local class="$1" classifier="$2"
  shift 2
  local failed=0
  local lane in_full in_web in_docs expected required result argument found known

  if [[ "${classifier}" != success ]]; then
    echo "pinned: the change classifier ended with: ${classifier}" >&2
    failed=1
  fi
  case "${class}" in
    full|web|docs) ;;
    *)
      echo "pinned: unknown change class: '${class}'" >&2
      exit 1
      ;;
  esac

  for argument in "$@"; do
    known=0
    while read -r lane _; do
      [[ -n "${lane}" ]] || continue
      if [[ "${argument%%=*}" == "${lane}" && "${argument}" == *=* ]]; then
        known=1
      fi
    done <<<"${LANES}"
    if ((known == 0)); then
      echo "pinned: unknown lane result: ${argument}" >&2
      failed=1
    fi
  done

  while read -r lane in_full in_web in_docs; do
    [[ -n "${lane}" ]] || continue
    case "${class}" in
      full) expected="${in_full}" ;;
      web) expected="${in_web}" ;;
      *) expected="${in_docs}" ;;
    esac
    case "${expected}" in
      run) required=success ;;
      skip) required=skipped ;;
      *)
        echo "pinned: lane table is malformed for ${lane}" >&2
        exit 1
        ;;
    esac

    found=0
    result=""
    for argument in "$@"; do
      if [[ "${argument%%=*}" == "${lane}" && "${argument}" == *=* ]]; then
        found=$((found + 1))
        result="${argument#*=}"
      fi
    done
    if ((found != 1)); then
      echo "pinned: expected exactly one result for ${lane}, got ${found}" >&2
      failed=1
      continue
    fi
    if [[ "${result}" == "${required}" ]]; then
      echo "pinned: ${lane} ${result} (class ${class})"
    else
      echo "pinned: ${lane} must be ${required} for class ${class}, but ended with: ${result}" >&2
      failed=1
    fi
  done <<<"${LANES}"

  if ((failed != 0)); then
    exit 1
  fi
  echo "pinned: every lane required for class ${class} succeeded"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  [[ $# -ge 1 ]] || usage
  command="$1"
  shift
  case "${command}" in
    classify) [[ $# -eq 0 ]] || usage; classify ;;
    detect) [[ $# -eq 0 ]] || usage; detect ;;
    verdict) verdict "$@" ;;
    *) usage ;;
  esac
fi
