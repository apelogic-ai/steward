#!/bin/sh
set -eu

if [ "${1:-}" = "--version" ]; then
  printf '%s\n' 'codex-cli 0.117.0'
  exit 0
fi

if [ "${1:-}" != "exec" ]; then
  printf '%s\n' 'unsupported deterministic Codex fixture invocation' >&2
  exit 2
fi
shift

output=''
prompt=''
while [ "$#" -gt 0 ]; do
  case "$1" in
    --skip-git-repo-check)
      shift
      ;;
    --output-last-message)
      output="${2:-}"
      shift 2
      ;;
    --)
      prompt="${2:-}"
      shift 2
      ;;
    *)
      printf '%s\n' 'unexpected deterministic Codex fixture argument' >&2
      exit 2
      ;;
  esac
done

if [ "$prompt" != 'Review the repository state that triggered this GitHub Actions run.' ]; then
  printf '%s\n' 'the immutable Workflow prompt was not supplied exactly' >&2
  exit 3
fi
if [ -z "$output" ]; then
  printf '%s\n' 'the standard result path was not supplied' >&2
  exit 3
fi

mkdir -p "$(dirname "$output")"
printf '%s\n' 'Repository review completed by codex@0.117.0.' >"$output"
