# DP-C01: Freeze direct-package v2 contracts

Priority: P0 foundation

Status: review pending in `apelogic-ai/steward#83` at exact head `320b79f`

## Goal

Define reviewable, provider-neutral schemas for direct invocation before the Identity,
Git source, Steward, runner, and package implementations proceed independently.

## Scope

- `steward.task/v2` invocation manifest with package repository, typed commit, path,
  Envelope digest, and optional diagnostics;
- direct TaskDefinition schema with prompt, runtime selection, outputs, optional
  skills, and optional complete `requires`;
- direct instruction-skill schema in which omitted kind means `instruction_only`;
- deterministic package-closure digest rules;
- verified source-provenance input shape;
- immutable Task source/effective-authority evidence shape; and
- successful-run stdout/stderr transcript filenames and limits.

The frozen `steward.m1/v1` schemas are inputs for compatibility tests only and are not
modified.

## Fixed semantics

- repository and commit remain separate fields;
- commit values are `git:sha1:<40-hex>` or the same-repository-only `git:trigger`;
- Envelope values are `steward:sha256:<64-hex>`;
- omitted or empty skills means no skills;
- instruction-only is the default and only initial skill kind;
- omitted `requires` expands to the selected Envelope's full approved values;
- present `requires` is a complete narrower authority request;
- nullable approved maxima remain explicit `null` in expanded evidence rather than
  being omitted or invented;
- omitted diagnostics means no caller-visible execution log; and
- `diagnostics.executionLog: full` requests successful stdout/stderr replay.

## Negative tests

- unknown contract versions and fields fail closed;
- malformed typed identities, mutable Git refs, and path escapes are rejected;
- `git:trigger` cannot identify a cross-repository package;
- duplicate paths, cycles, cross-source dependencies, and non-canonical encodings are
  rejected;
- unknown skill kinds cannot execute;
- partial present `requires` and ungrounded execution-capability requests are invalid;
  and
- unknown diagnostic modes cannot enable logging.

## Exit criteria

- schemas, canonicalization, digest vectors, positive fixtures, and negative fixtures
  are reviewed;
- generated artifacts, if any, are produced through the repository's normal generator;
- compatibility tests prove frozen v1 is unchanged; and
- DP-I01, DP-G01, DP-R01, DP-A01, and DP-S01 can consume the contract without
  inventing local field meanings.

## Parallel boundary

This ticket lands first. The four independent implementation lanes may start from its
reviewed contract commit. Wire-compatible corrections are coordinated here rather than
made independently in consumers.

## Implementation evidence

- initial checkpoint: `042e8aa`;
- authority-evidence correction: `d4d8dc4`;
- v2 status evidence exposure: `0455bf3`;
- rebased post-maintenance head: `320b79f`;
- focused contract tests, all Steward type tests, Clippy, formatting, all 27 JSON
  Schema fixtures, status/evidence consistency regressions, and diff checks: green;
  and
- full `cargo xtask ci`, pinned conformance, and the pre-push full quality gate are
  green after isolated neutrality maintenance PR `apelogic-ai/steward#82` merged as
  `cc19487`.
