# DP-R02: Remove build-only GLib chain from the Steward runner image

Priority: P0 blocker

Status: ready to implement in a separate `apelogic-ai/steward-run` PR

## Goal

Restore the runner image's critical-vulnerability gate without accepting an unfixed
finding, weakening the policy, or mixing baseline remediation into DP-R01.

The unchanged pinned Actions runner base now reports CVE-2026-58016 in four GLib
binary packages. The current Ubuntu package feed has no fixed version, and the latest
official Actions runner image contains the same affected package versions. The chain
is introduced through `software-properties-common`, which is needed only while the
upstream image configures its Git package source.

## Scope

- keep the existing Actions runner image tag and digest;
- after Git source setup has completed, explicitly purge
  `software-properties-common` and the narrowly resolved dependent chain containing
  the four affected GLib packages;
- never use `--autoremove` for this purge;
- assert during the image build that the four affected package names are absent; and
- retain the existing thin-runner, tool, and runtime invariants.

The PR must not edit the vulnerability policy, add an acceptance, reduce scan
coverage, bump the runner base, or include direct-package behavior changes.

## Regression and security tests

- begin with a failing assertion that the final image contains none of
  `gir1.2-glib-2.0`, `libglib2.0-0t64`, `libglib2.0-bin`, or `libglib2.0-data`;
- prove `apt-get check` succeeds after the explicit purge;
- prove Runner.Listener starts and has no missing linked libraries;
- preserve Git, jq, Python, unzip, passwordless runner sudo, Docker CLI, and buildx;
- run the existing thin-shell and container smoke checks; and
- run the live Trivy policy gate against the final image.

## Exit criteria

- a separate `apelogic-ai/steward-run` PR contains only the narrow image remediation
  and its assertions;
- the image contains none of the four affected packages;
- all runner/tool invariants and repository gates are green;
- the live vulnerability report contains no unaccepted critical-class finding; and
- after human merge, DP-R01 incorporates the remediation through normal shared-branch
  history without rewriting it.

## Evidence before implementation

- DP-R01 PR artifact: four CVE-2026-58016 findings at package version
  `2.80.0-6ubuntu3.8`, with no fixed version;
- remote `main` and DP-R01 have byte-identical Dockerfile, CI, evaluator, and policy,
  so the last green `main` scan is stale relative to the current feed;
- the latest official Actions runner image still contains the same affected package
  versions, so a runner bump alone is not remediation; and
- an ephemeral targeted-purge proof retained the named runner/tool invariants and
  removed all four affected packages.

## Parallel boundary

May proceed independently of DP-C01, DP-I01, DP-G01, DP-S01, and DP-A01. It blocks a
green DP-R01 CI result. No Kubernetes environment is required.
