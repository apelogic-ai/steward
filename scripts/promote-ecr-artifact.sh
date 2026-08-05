#!/usr/bin/env bash
set -euo pipefail

if [[ "$#" -ne 5 ]]; then
  echo "usage: $0 <source-ref> <registry> <repository> <tag> <expected-digest>" >&2
  exit 2
fi

source_ref="$1"
registry="$2"
repository="$3"
tag="$4"
expected_digest="$5"
target_ref="${registry}/${repository}:${tag}"

if [[ ! "${expected_digest}" =~ ^sha256:[a-f0-9]{64}$ ]]; then
  echo "invalid expected digest for ${target_ref}" >&2
  exit 2
fi

digest_for_tag() {
  jq --raw-output --arg tag "${tag}" \
    '[.imageDetails[]? | select(any(.imageTags[]?; . == $tag)) | .imageDigest][0] // empty'
}

describe_repository() {
  aws ecr describe-images --repository-name "${repository}" --output json
}

existing_digest="$(describe_repository | digest_for_tag)"
if [[ -z "${existing_digest}" ]]; then
  echo "missing target ${target_ref}; publishing ${expected_digest}"
  oras cp "${source_ref}" "${target_ref}"
elif [[ "${existing_digest}" == "${expected_digest}" ]]; then
  echo "matching target ${target_ref}; reusing ${expected_digest}"
else
  echo "different digest at ${target_ref}: expected ${expected_digest}, found ${existing_digest}" >&2
  exit 1
fi

published_digest="$(describe_repository | digest_for_tag)"
if [[ "${published_digest}" != "${expected_digest}" ]]; then
  echo "target verification failed for ${target_ref}: expected ${expected_digest}, found ${published_digest:-missing}" >&2
  exit 1
fi

printf '%s\n' "${published_digest}"
